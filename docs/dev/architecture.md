# 架构与数据流

## 三层结构

```
Frontends (wing 二进制)          Gateway (FastAPI)          Runtime (Python)
─────────────────────           ─────────────────          ─────────────────
TUI 模式（默认，ratatui）   ──►  GatewayServer          ──►  WingRuntime（协调者）
stdio 模式（wing -p，NDJSON）◄─  · routes/session/system     ├─ SessionManager（多会话）
Web 客户端（apps/web，浏览器）    · routes/ws（事件流）        ├─ SessionStore（持久化）
                                 · routes/workspace（图片）   ├─ ContextManager（压缩/回退）
GatewayClient(WS) + ApiClient    · auth（opt-in）            ├─ EventBus（事件路由）
                                 · 静态托管 / CORS（opt-in）  └─
```

- **Runtime** 是服务层协调者，本身不实现业务：路由 handler 只做参数校验 + 构造响应，逻辑下沉到 `Session` / `ContextManager`（PR #14）。
- **Gateway** 是 FastAPI 进程，持有 `EventBus` 订阅，把 Runtime 产生的事件经 WS 推给已订阅客户端。
- **Frontends** 都在 `wing` 二进制里，共享同一套 Gateway + Runtime；`extensions/vscode`
  与 `apps/web` 是另外两个（后者的构建产物由 Gateway 自身托管：`gateway.static_dir` 配置后
  网关就是 Web 服务器，未命中路径 SPA fallback 到 `index.html`，`/api/*`、`/ws`、`/docs`
  永不 fallback——见 [http-api.md](http-api.md#静态托管与开发期-cors)。

## 数据流（一次对话）

```
用户输入
  → HTTP POST /api/session/send
  → WingRuntime → Session.agent loop（LLM 调用 + 工具并发执行）
  → 产生 ReAct 事件 → EventBus
  → Gateway 经 WS 推给订阅者
  → 前端 handle_event → 状态变更 → 渲染
```

工具调用经 `asyncio.gather` **并发执行**（PR #33）；工具结果以 `tool_call_id` 精确回填，避免并发 Ask 时的饥饿（PR #37）；超长结果内置截断（头尾保留 + 全文落临时文件，PR #19，见 `tool_result_truncate` 配置）。

工具参数 JSON 解析是容错契约（`provider/base.parse_tool_args`，**永不抛**）：笨模型吐出非法 args（尾逗号、非 object 等）时不触发整轮重试——那会丢弃已生成的 thinking/content/tool call——而是置 `arguments={}` 并在 `ToolCall.arguments_error` 记录错误现场（含完整原始文本），`ToolExecutor` 见它短路执行（不走工具、不走 hook），把错误作为工具结果回灌给模型自纠。回放时该 call 序列化为 `{}` 参数，教学信息由 tool result 承载。

## 三种前端形态

### TUI 模式（默认）

`wing`（无 `-p`）进入 ratatui 事件循环。核心是 `App` 状态机：

- 用户操作 / 命令 → `AppIntent`（意图，无副作用）→ `runner.rs` 执行（发起 HTTP/WS）；
- WS 事件 → `handle_event()` → 状态变更 → `draw()` 渲染。

斜杠命令大多被前端拦截转 HTTP（如 `/compact`→`POST /api/session/compact`）；`/clear`、`/copy` 纯前端本地处理；用户 `.md` prompt 命令展开为文本后作为普通消息发送（见 glossary「命令」）。

**流式增量渲染**：Reasoning / assistant 长文本的 delta 不再每帧全量重渲染（O(n²) 整轮）。`render/markdown/stream.rs` 的 `StreamingRender` 把流式文本切成 markdown 块——已闭合块渲染一次提升为不可变稳定前缀，每帧只重渲染活动尾部；未闭合代码块走行级缓存（Content 保留 syntect 有状态高亮、Thinking 永久 plain）。`CachedCell` 的流式分支不 bump generation（细粒度失效），渲染循环对预折行 cell 直接逐行 blit（去 `Paragraph` Composer 与 clone），高度 O(1)。WS 事件只置脏，draw 由 16ms 帧间隔合帧（输入旁路节流）。turn 结束 `finalize` 全量对账兜底任何增量漂移。基准与对账矩阵：`crates/wing/benches/stream_render.rs`、`tests/stream_render_{reconcile,throughput}.rs`（512KB 平均帧 18.2ms→13µs，p99<0.6ms，支撑 3000 tokens/s）。

**工具参数（`tool_call_stream`）是同一模式的第二个实例**：片段追加退化为 O(1)（只 push + 置 dirty），解析 / 语法高亮 / 渲染缓存失效推迟到帧边界每格至多一次（`CachedCell::compute_*` → `flush_pending_args`；`is_final` 与权威 args 强制冲刷）——旧的 per-fragment 全量重解析是 O(n²)，会把 256 有界事件通道顶满。基准：`crates/wing/benches/tool_args_stream.rs`（append ~20–40ns/片段且与 payload 无关；帧成本 ∝ payload、每帧一次）。

### stdio 模式（`wing -p`，PR #1）

无 human-in-the-loop 的头模式，**兼容 Claude Code 的 NDJSON 协议**——把 `wing` alias 为 `claude` 即可接入现有编排生态。

```bash
wing -p "列出文件"                          # text（默认）：仅输出最终结果
wing -p "列出文件" --output-format json      # json：单个 result 对象
wing -p "列出文件" --output-format stream-json  # 实时 NDJSON 流
```

`stream-json` 消息类型：`system/init`（tools/model/cwd）· `assistant`（content blocks + usage）· `user`（tool_result blocks）· `result`（终止信号：累计 usage / turns / 耗时）。支持 SDK 双向 stdin 握手（`--input-format stream-json`）。**未识别的 `--xxx` 参数被静默忽略**，确保外部编排层传递的 Claude 专有参数（如 `--permission-mode`）不报错。

> 在后台执行 `wing -p "request" > /tmp/result.md` 等价于调度了一个拥有任意命令执行权限的子 agent。多 agent 不易驾驭，yolo 本身危险，编排者应审慎使用。

### 编排 CLI（`wing run` / `wait` / `ps` …，PR #64）

面向脚本与外部编排器的非交互子命令，全部走 HTTP；`--json` 输出可直接被 agent 消费：

- `wing run "<prompt>"`：建会话 + 发送 prompt 后**立即返回 session id**（非阻塞）；`wing wait <sid>…` 阻塞至会话进入 idle/inactive（HTTP 轮询 + WS `TurnResult` 双通道，`--timeout` 兜底）。事件流终止（帧超限 / Close 帧 / 读错误）时**立即报错退出**（stderr 含关闭原因与未完成 session，非零退出码）——不空转、不静默降级为纯 HTTP 轮询；细节见 `gateway/client.rs` 的 `CloseReason`；
- `wing ps [--all] [--watch]` / `wing info <sid>`：会话列表 / 单会话运行时信息（model、tools、tokens、status）；
- `wing tail|head <sid> -n N -t <type>`：消息窗口（类 Unix head/tail；平铺元素模型——按 user/assistant/tool_call/tool_result/reasoning/content 选取元素并在输出侧剥离，文本与 `--json` 一致（例外：tool_result 文本模式为 500 字符 peek、`--json` 为存储全文；`all` 保持原样 payload））；
- `wing models|tools|agents`：系统查询；`wing start|stop|status`：网关守护进程生命周期（默认的 TUI / stdio 启动路径会自动拉起网关）。

## Goal 编排（TUI-side，PR #22）

`/goal <prompt>` 启动：**executor**（当前会话）执行任务，独立的 **checker** 会话（受限只读工具集 Bash/Read/Glob/Grep）独立验证并给出 `<goal_finish>` 判定；循环往复直到 checker 确认达成——**让 agent 不再自评自己的成果**。

- 编排在 Rust TUI 内，`goal.rs` 是**无 I/O 的纯状态机**（每次转移返回 `Vec<GoalAction>`，App 翻译为副作用），无后端改动，可干净移除。
- 每轮向两个 agent 发送**完整**结构化 prompt（【任务目标】+【追加信息】），避免压缩漂移。
- TUI 内编排的限制：不持久化（重启 TUI 丢失）、无迭代上限、重连不自动恢复。`/goal-exit` 退出。后台 + 持久化版本见 `wing-orch`（下节）。

## 远程工具与编排（PR #47 / #49 / #50）

工具不必跑在 gateway 进程里。外部 **tool host** 经 HTTP 注册工具、经一条常驻 WS 服务调用，让工具可跑在另一台机器 / 容器 / 编排器里——这是迈向外部编排（`wing-orch`）与端到端软件交付的第一步。

**dispatch 模型（闭包，非事件 RPC）**：注册时 gateway 为每个远程工具构造一个捕获 `(RemoteToolManager, client_id, tool_name)` 的 async 闭包，装进 `Tool.function`。agent 调用时闭包生成 `call_id`，经该 client 的 WS 发 `tool_call_request` 帧，await 一个由 WS 读循环在匹配 `tool_call_result` 帧时 resolve 的 future。请求/响应关联全在 manager 的 future 表里——EventBus（仅出站通知）不被打扰，**核心完全网络无关**。

**身份与权限解耦**：任何角色都可在 WS 连接经 `?client_id=<id>` 自选 client_id（先来先得到、全量唯一性校验、`default` 保留），声明 client_id 即具备注册资格。RBAC 角色独立：`admin` 全量，`tool_runtime` 纯工具执行（仅 `POST /api/tools/register` + 工具 WS，不收事件、不能发用户消息）。

**生命周期与缓存保护**：断连是首要失败信号——`fail_client` 立即失败在途调用并注销工具；`gateway.remote_tool_timeout`（默认 1800s）仅为安全网。**KV cache 保护**：断连只清全局 registry，绝不动已加载 agent 绑定的工具表——持有的工具引用仍可调用，清晰返回 "not connected"。

**动态工具切换（PR #50）**：工具集可运行期经 `POST /api/session/update`（`tools`）变更。难点在于改 OpenAI `tools` 字段会使服务端 KV 前缀 cache 失效。策略（唯一事实 `Agent._tools` + 唯一入口 `set_tools()`，policy 下沉到拥有链状态的 ContextManager）：

- **冷**（链空 / 首次初始化）：declared 与可执行集同步更新，无提醒。
- **热**（链非空）：declared 冻结，注入 user-role System Reminder（完整工具 schema + namespace 标注）。
- **压缩后**：declared 自动同步（前缀 cache 本就已失效）。

**SDK 与编排**：Rust `wing-api-client::tool_host`（builder）与 Python `wing-sdk`（decorator）让注册/服务工具原生化。`wing-orch goal` 把 Goal 状态机从 TUI 抽出为独立后台进程：无限轮次、yolo（远程工具无审批路径）、原子状态持久化 + `--resume`，终端关闭不死。

## 会话生命周期

| 操作 | 端点 | 要点 |
|------|------|------|
| create | `POST /api/session/create` | 选 `backend: file\|memory`；session id **由后端生成**（客户端不得指定）；`before_session_start` hook 跑完后把追加系统提示词落盘 |
| resume | `POST /api/session/resume` | 同一个 session id 换入内存：还原 template_name + workspace + 模型绑定 + 提示词与动态状态（metadata 记录优先于模板/配置默认，见 glossary「持久会话状态」）；**不触发** `before_session_start`（不是新会话） |
| fork | `POST /api/session/fork` | 写完整 metadata（workspace/forked_from/template + 源生效状态快照：模型 / 提示词 / 工具 / 开关），继承源 backend；**子会话 = 源记录前缀**（append 顺序，日志级拷贝 + uuid 全量重映射；活跃链由 tip 回溯自然得出，压缩节点即止——被压缩区间随行但不活跃，即"回得去"）；**属于创建新 session**（新 session id）→ `before_session_start` 在子会话上生效 |
| rewind | `POST /api/session/rewind` | 丢弃指定消息之后的内容 |
| compact | `POST /api/session/compact` | 委托 `ContextManager.do_manual_compact()` |
| interrupt | `POST /api/session/interrupt` | 委托 `Session.agent.interrupt()`；打断对账见下 |
| release | `POST /api/session/release` | 逐出内存态（显式 eviction）；钉住条件不满足时 409 |

### 会话逐出（eviction）

**内存态是缓存**：`SessionManager._sessions` 是"在场会话"的工作集，磁盘（`history.jsonl` + `metadata.json`）是唯一事实来源。逐出 = 让该会话经历一次"gateway 重启"——只回收运行期资源（worker task + provider client），磁盘一概不动。

| 维度 | 口径 |
|------|------|
| 判定 | 三条全过才逐出：`status == idle`（working / waiting 钉住）、inbox 无待处理输入、无 client 订阅（EventBus 路由表）、空闲时长 > `sessions.eviction.idle_ttl_seconds` |
| 硬条件 | inbox 有待处理输入不逐出（`agent.post()` 直投路径不 touch 计时器）；有后台任务（后台 Explorer）不逐出——拆解会关掉它共享的 provider；memory 后端不逐出（逐出 = 数据销毁） |
| 计时 | `touch` = 任何携带该 session_id 的事件（`SessionReaper` 订阅 EventBus）——"会话状态变化即重置计时器"；create / resume 初始化 |
| 触发 | `BackgroundScheduler`（gateway lifespan 启停）周期扫描（`sweep_interval_seconds`，启动时读取）；`release` 立即判定（忽略空闲时长，不忽略钉住条件） |
| 拆解 | pop 同步原子摘除 → `Session.aclose()`（`agent.shutdown()` + `aclose_providers()`，顺序固定）异步收尾 |
| 水合 | 被逐出 ≠ 不存在：`resume` / `subscribe` / `send`（HTTP 与 WS 上行）按需水合；空会话（无消息、无磁盘痕迹）逐出后不可恢复 |
| 可见痕迹 | `/api/session/list` 的 `status: inactive` 是主信号；此外 `session/get` / `info` / `branches` 对已逐出会话回 404（`wing tail` / `head` / `info` 内部 404→resume），`release` 返回 `not loaded` |

`BackgroundScheduler`（`wing/background.py`）是通用周期任务宿主（单 task 顺序执行、job 异常隔离、start/stop 显式），逐出只是第一个 job——将来的后台机制（dreaming 等）直接 `add_job`。`SessionReaper`（`wing/session_reaper.py`）只做"触摸订阅 + 一次扫描"，不依赖调度器即可单测。

**不做的**（刻意缺席）：容量上限 / LRU（只有 TTL）；逐出以外的入口不自动水合（`session/get`、`info`、`branches` 对已逐出会话仍 404——`wing tail` 的 404→resume 是既有惯例，需要时按同一模式补）；动态状态（tools 热切换的冻结声明集、thinking / yolo）不落盘，逐出后按模板默认重建（与重启同语义）。

### 中断提交语义

**不变量**：上下文中每个带 `tool_calls` 的 assistant 消息后必须跟齐每个 call_id 的 tool 消息——悬空 `tool_calls` 会被服务端拒绝，破坏 session。

面向现代模型的生成时序（reasoning → content → tool_calls，tool 流完响应才结束），打断分四种情况，其中两种需要对账：

- **流式三段**（reasoning / content / tool 参数流式）中打断：`_call_llm` 在 `except CancelledError` 中从 caller 持有的 accumulator 快照已累积的部分块——text/thinking 任意长度保留（半截无害），**未终结的 tool 块由 provider 剔除**（半截参数不可解析，且无配对结果会破坏不变量）——组装 partial assistant Message（`stop_reason="interrupted"`）提交进上下文，然后 re-raise。用户可放心打断长思考：已花费 tokens 的内容不丢，模型下轮请求能看到自己的半成品。
- **工具执行中**打断：LLM 响应已完整。`interrupt()`（async）cancel 旧 worker 并 **await 其拆卸完成**后重建 → 旧 worker 的 `exec_tool_calls` 在 `except CancelledError` 中收拢每个 call 的最终结果——已完成的取**真结果**，被取消的合成一句话结果（`"Tool call interrupted by user."`）——经 `_InterruptedToolResults` 抛给 `_llm_turn`，本轮消息沿**正常路径** `add_messages` 提交，然后重新抛出**原始** `CancelledError`（保留取消调用栈）让 worker 终止（不重抛则取消被消化，agent 会继续跑下一轮 LLM）。`shutdown()`（switch_template）走同一路径，免费获得同样保证。

**provider accumulator 协议**：`ModelProvider.generate(..., accumulator=)` 接受 caller 注入的不透明容器（`StreamAccumulator`），每次尝试（含重试）开始时填充新状态——取消后 caller 仍可经 `snapshot_blocks()` 读取已累积内容。`with_retry` 只捕 `Exception`（CancelledError 是 BaseException），取消直通，不会被重试吞掉（`test_with_retry.py` 钉死）。

**三段超时口径（响应头 / 响应体停滞 / 总时长）**：`timeout_first_chunk` 只包住 `send(stream=True)` 即**响应头**，`timeout_total` 是总时长；响应体自身的停滞由 `provider/transport.py` 的 `lines_with_idle_timeout()` 判定——**硬编码 120s**（`STREAM_IDLE_TIMEOUT`，无配置项），两次读取间隔超过它即判停滞并抛 `TimeoutError`，交由既有 `with_retry` 重试（不另写重试逻辑）。两个 provider 的响应体循环共用它。日志文案与新事件口径：收到响应头记 `response header received`（不再谎称 `stream call connected`），重试通知走 `notice` 事件（见下节）。

**无效轮次与自动重试**：单轮生成若「不进入下一次 ReAct 且不合法」——（a）无 content 且无收敛（已终结）的 tool call（含全空与只有 reasoning——reasoning 不作为收尾依据），或（b）有 content、tool call 起了头但一个都没收敛（流被上游截断，如网关在非法 JSON 工具调用处直接切断；流未正常结束、无权威块数组的形态——如 Anthropic 在 `message_stop` 前被切断——同判）——判定为**无效轮次**（`ReActLoop._call_llm_validated`，抛 `InvalidGenerationError`）：（a）不提交任何内容；（b）提交 content、不提交 tool call（与 provider 剔除未收敛块的口径一致）。随后交给 `with_retry(retry_on=(InvalidGenerationError,), label="模型生成")` 有界重试（参数经 `ReActLoop._config` 跟随**当前 provider 配置**的 `max_retries` / `max_retry_delay`；重试通知走 `notice`）。**有任一收敛 tool call 则永不重试**（自然进入下一轮）。截断检测用 `unfinished_tool_calls()` 计数（含无 id 的半截调用，无盲区）；`retry_on` 过滤保证语义重试不与 provider 层的传输重试叠加放大；无效尝试置空 `_current_acc`（被丢弃的内容不进未提交投影），其 usage 照常计入 turn 账。重试耗尽沿 turn 错误路径上报（`turn_result` error + `error` 事件）：无效轮次绝不作为成功 turn 提交。规则（b）会在链上产生相邻 assistant 消息——Anthropic 序列化器按既有交替规则**合并连续同角色消息**（等价于未截断时「text + tool_use 同一条」的形态）；已知边界：该轮的重试请求以 assistant 结尾（续跑语义），Anthropic 开 thinking 时 prefill 是否被拒待真实环境验证（被拒则该轮重试失败、内容不丢）。

**stop_reason 捕获**：两个 provider 均在最终 usage 携带协议原值（`end_turn`/`max_tokens`/`tool_use`/`stop`/`length`），传导进 `Message.stop_reason`（**唯一落盘审计位置**）与 `LLMCallMetricsEvent.stop_reason`（仅用于直播——该事件 persist=false，不落盘；metrics_registry 经 event_bus 聚合进独立的 metrics.json）。Anthropic 的 max_tokens 砍在 tool args 中间时，未终结的 tool 块从权威块数组剔除（半截 tool_use 不再被当作完整调用执行）；该剔除与未提交投影共用同一实现（`_ordered_finalized_blocks`）。

合成结果同时发射与正常完成相同的 `ToolCallResultEvent` / `ToolResultTurnEvent`：TUI 据此翻转 cell 状态（Bash 计时器仅在 cell 为 Pending 时前进，结果事件使其冻结——修复了打断后计时器不停的存量问题），stdio 模式据此输出 user turn 消息。

**时序**：runtime 先 await `agent.interrupt()`（补提交随之完成）再 emit `InterruptedEvent`（persist=true，落盘于 partial Message 之后，链序正确）——客户端观察到 Interrupted 时 store 已一致。收尸 gather 带 5s 兜底超时，行为不端的工具（吞掉取消）不会无限挂起补提交路径。

## 事件系统与统一日志

**事件是唯一事实来源**：history.jsonl 是混合日志，承载两种记录——Message 记录（`role ∈ user/assistant/tool/system`，进入 LLM 上下文）与事件记录（`role="event"`，携带事件自身字段）。所有记录共享链拓扑（uuid/parent_uuid），事件参与链构建；`Message` 与前端 `ChatView` 都是同一日志的投影。记录级判别只用 `role`。

**未提交内容的单一权威 = provider accumulator**：turn 进行中"已生成但未提交"的内容只有一处权威——caller（`ReActLoop`）持有的流累积状态（`_current_acc`，一轮 LLM 调用生命周期：`_call_llm` 入口新建、轮提交/中断补提交/turn 收口后置空）。对外两个覆盖互斥的投影，按需快照、不缓存副本：

- `snapshot_blocks(acc)`：**已终结**块（text/thinking 任意长度保留，未终结 tool 块剔除）——中断补提交与未提交 Message 投影**同源**（resume 与打断变成同一个操作）。
- `pending_tool_calls(acc)`：**未终结** tool 调用的原始 args 文本（`PendingToolView`）——活工具卡渲染素材，后端不解析半截 JSON（局部解析在客户端 `partial_json.rs`）。截断检测走 `unfinished_tool_calls(acc)` 计数口径（含无 id 的半截调用，见上节「无效轮次与自动重试」）。

**落盘只存事实，不存副本**：`persist=true` 事件（diff/ask/interrupted/error/compact_done）在完整产生时即时落盘进链；流式 delta（`persist=false`）纯广播——不落盘、不进任何内存缓冲，其内容由轮提交时的 Message 记录承载。`tool_call_result` / `llm_call_metrics` 是 Message 孪生（`role="tool"` Message / `Message.usage` + `Message.stop_reason`），已停止落盘（事件本身保留：metrics_registry 与 TUI 直播经 event_bus 依赖）；`turn_result` 保留落盘但 `result` 字段（最终文本孪生）经 `disk_exclude` 排除出磁盘记录（wire 帧仍携带，stdio 消费）。

**persist 分流原则**：`WingEvent.persist` 是 `ClassVar[bool]`（非 pydantic 字段——旧的 `Field(exclude=True)` 会被子类重声明静默击穿），基类默认 true。判据两条同时成立：**是事实**（读回来仍成立，非一次性信号）**且无 Message 孪生**。false 仅限（a）流式 delta，（b）与 Message 完全孪生且体积可观的事件（AssistantTurn/ToolResultTurn/ToolCall/ToolCallResult/LLMCallMetrics），（c）可从权威状态实时重建的协议/查询事件（Sync/SessionInit/ContextStats/BranchTargets/Delivered/SessionStateChanged），（d）一次性通知（`notice`——如「LLM 调用失败，N 秒后重试」）。

**error 与 notice 的语义边界**：`error` = 真错误（前端终结 turn、渲染错误单元、未聚焦时 OSC 9 通知）；`notice` = 提醒（`level` + `message`，不终结 turn、不发通知、不落盘）。重试通知走 `notice`——重试中的 turn 并没有结束，而 `error` 在前端是「这一轮结束了」的**终结信号**（曾把「会自愈的失败」误报成真错误：spinner 停、耗时停、状态错位）。

**三个序列化边界，三套剥离规则**（不可用一个 `model_dump(exclude_none)` 打通——`Message._serialize_flat` 是 wrap 序列化器，`content`/`reasoning_content`/`tool_calls` 在内层 handler 之后才注入）：磁盘记录（`TrackedList._to_record`：字典推导剥 null + 剥 target + disk_exclude）、WS 直播帧与 SyncSession 载荷（`event/__init__.py::wire_dump`：剥 null + 剥 `parent_uuid`/`unzip_last_uuid`/`role`/`target`，保留 `uuid`）。`serialize_event` 是 `wire_dump` 的别名——一套规则，无分散实现。

**rewind/fork/compact 凭链序免费工作**：事件是链节点——set_tip 后边界外事件移出活跃链；fork 拷贝混合链前缀（事件随行）；compact 后被压缩区间的事件与消息一并离开活跃链（被压缩的 diff 自然不再重放，无孤儿处理）。

**中途订阅完整视图**：`_push_sync` 的 SyncSessionEvent 是**状态 + 素材**：`status`（快照时刻的运行状态 idle / working / waiting，**权威**）+ `turn_started_at`（恢复 working 已耗时）+ 四组重放素材——`messages`（已提交 Message 投影）+ `uncommitted`（**单个**未提交 assistant Message 投影，已终结块）+ `uncommitted_tools`（未终结调用的原始 args 片段）+ `events`（活跃链上的**事实类**事件，按链序）。前端组装顺序 **messages → uncommitted → uncommitted_tools → events → live 流**：`messages` 与 `uncommitted` 走同一条 `replay_messages` 路径，`uncommitted_tools` 走既有 live `ToolCallStream` 分支（客户端局部解析），`events` 最后锚定。这个顺序让"diff 渲染在其 ToolCall 卡片之前"的缺陷结构性消失——产生 diff 的 tool_use 是已终结块，必在 `uncommitted` 投影里先建出锚点 cell。`events` 的下发过滤归属后端单点（`FACT_EVENTS` + pending ask 谓词，见下）。

**working 状态由 `status` 回答，MUST NOT 由内容反推**：中途订阅者听不到已经过去的 `turn_started`（一次性 live 事件，`persist=false` 不落盘、不重放，`Done` 之前不会再有第二个），所以"在不在跑"必须由快照显式给出。"有未提交内容 ⇒ working"只是**单向**成立：一轮 LLM 调用在飞行、尚未吐出任何已终结块时（首帧未到——含 TTFT 与 `get_messages_for_llm` 里 await 后台 compact 的窗口；以及每个多轮 turn 都要穿过的**轮边界**）两个投影都是空的，而 turn 确在 working。旧实现据内容推断，这两个相位下 resume/join 会把 working 读成 idle——spinner 不转、标题不切、耗时不计，live 事件却照常渲染（它们不经过 `turn.working`）。`status` 取自 `Session.status`（优先级 waiting > working > idle），前端把 working / waiting 都当"turn 在飞行"（与直播一致：ask 挂起时 spinner 照转），idle / inactive 则清掉残留的 working（视图被切会话 / 重连替换时，上一轮的 `Done` 已被跨会话过滤吃掉）。

**没有"回落推断"这条路径**：`status` 必填且取值严格（Python 必填字段 + 两端解码期 fail-fast）——CLI 与网关同版本升级，字段缺失 / 取值未知即协议错误，不在前端猜。两端也不读 `turn_started_at` 判状态（它是耗时锚点，不是生命周期标志）。

**pending ask 重放**：`AskEvent` 落盘，但已答的 ask 不再是关于当下的事实（重放会渲染活的 Ask 卡，回答找不到 waiter 进虚空）。后端用 `Inbox._feedback_waiters` 的键集合（`pending_ask_ids()`）作权威待答集合，`get_active_events()` 据此只下发仍挂起的 ask；前端重放渲染 Ask cell 并注册可答状态（归一化入口 `AskPanel::from_ask` 产出的唯一模型），用户回答走既有通道 resolve waiter。

**前端重放**：`replay.rs` 两段式——先 replay_messages（建 ToolCall/thinking/text cell），再 replay_events（**能力分发**：diff 按 tool_call_id 锚定插入对应 ToolCall cell 后、ask 渲染为可答卡片，无渲染器的类型跳过以保持前向容忍；未知锚点 fallback append，与直播路径同款兜底）。前端**不再编码"孪生事件不得渲染"这类策略**——过滤已在后端 `FACT_EVENTS` 完成，前端只做能力分发。

**diff 载荷是窗口，不是文件**（`diff-payload-window`）：`DiffContentEvent.old_text/new_text` 只携带**变更区域 ± 3 行**（`tools/diff_window.py` 的 `DIFF_CONTEXT_LINES`），外加窗口首行在各自修订版中的 1 起绝对行号 `old_start_line` / `new_start_line`（缺失按 1——旧载荷与旧网关照此渲染）。Write / 新建文件仍发全量（`old_text=None` 即"全是新增"）；`replace_all` 每个匹配位置一条事件（同一 `tool_call_id`、按位置升序），前端按既有锚点顺序逐个插入 diff cell。前端**不折叠**：`DiffView` 逐行渲染后端给的窗口，gutter 行号与 `@@` 头由 `*_start_line` 推算（`LayoutConfig.diff_context` 已删除，前端不再有"上下文行数"这个策略旋钮）。窗口化把 diff 载荷从"随文件大小"降到"随变更区域大小"（实测 67 条事件 10.1 MB → 0.21 MB、渲染行 121k → 2.0k）；历史会话里已落盘的全量事件不迁移，在前端就是"一个覆盖整文件的窗口"。

**newest.json 已移除**：快照的消费者（重放）由混合日志承担；存在性判据与标题回退收敛为 history.jsonl。磁盘遗留的 newest.json 不读不写不删。

## 持久化（PR #39）

`SessionStore` 是会话**唯一**的持久化所有者（metadata + message log + aux）。历史上四个独立写者导致 fork 丢 workspace 等结构性 bug，现统一收口。

```
Session / SessionManager
        │
SessionStore (ABC) ── load/save_metadata · open_log → MessageLog · list/resolve
  ├── FileSessionStore     (~/.wing/core/sessions/，混合日志零迁移)
  └── MemorySessionStore   (进程内，不落盘)
  └── [未来: Sqlite / Pg / Supabase / Redis —— 增量实现，非架构改动]

TrackedList = 纯内存链拓扑引擎（uuid/parentUuid、trace、find、set_tip），
              ChainNode 家族混排（Message + WingEvent）；所有 I/O 委托
              MessageLog；log=None 即纯内存
MessageLog  = 追加式混合记录 + aux kv（pending compaction 存于此）
```

接口与存储无关：`SessionStore` 6 方法 + `MessageLog` 5 方法，全是 kv / append-only / listing 语义，不泄漏路径 / fsync / glob。一个 PG 后端就是三张表（sessions / messages / aux）。

**存量 session 零迁移**：老日志无事件记录 → `TrackedList.load` 按 role 分发（事件走 `EVENT_TYPES` 注册表，未知 type 跳过——前向容忍），事件重放为空即回退纯消息重放。不承诺老版本代码读新格式日志（新 session 由新代码产生，该场景不存在）。

## 压缩与缓存哲学

- **不在上下文中施加魔法**：从不注入隐藏 system prompt。
- **理论最高缓存命中率**：除压缩外绝不破坏缓存前缀；`explicit_cache_mode` 可为支持的 provider（如 DashScope）追加 `cache_control` 标记（PR #17）。
- **前缀身份 = 会话状态**：一次请求的"前缀"不止消息——`system` 段（含 `append_system_prompt`）、`tools` 声明、影响服务端处理的开关（`enable_thinking` / `preserve_thinking` / `reasoning_effort`）都参与缓存身份。因此这些会话级状态全部持久化在 `metadata.json`（`system_prompt` / `append_system_prompt` / `tools` / `thinking` / `reasoning_effort` / `yolo` / `max_turns`），resume / fork 重建 agent 后逐字节复现——否则重建后的请求从第 0 个 token 起就与重建前不同，整个上下文无法命中缓存。`append_system_prompt` 由 `before_session_start` hook（如 workspace_env_inject）与 `AgentOverride.append_system_prompt` 共用同一字段写入、随创建落盘。

**hook 的触发条件是 session id 是否变化**：create 与 fork 都在创建新 session（新 id）→ `before_session_start` 生效；resume 是同一个 id 换入内存 → 不触发，`append_system_prompt` 由持久记录还原（这就是"逐出 / 重启后 resume 不再丢提示词、也不再碎前缀"的修复点）。fork 的复制口径：提示词 / 工具集 / yolo / max_turns 取源会话此刻的 **live 有效值**（子会话先继承，`before_session_start` 随后在新会话上叠加注入——不自幂等的 hook 会叠出一层重复内容，属钩子自身的问题，钩子系统重做时收口）；`thinking` / `reasoning_effort` 只拷**显式记录**——provider 派生默认值（如 anthropic 未配置 thinking）不得被固化成子会话的显式配置，否则请求体会带上源会话没有的字段。

**fork 不承诺前缀复用**：子会话是新的 session id（`prompt_cache_key` 随之变化，见下），上游缓存本就要重建；这套语义只在 **resume**（同一个 session id）上追求逐字节复现。注意区分两件事：fork 的**记录口径**（拷贝范围）与**活跃链口径**（上下文）——前者保"回得去"，后者保"不复活"。
- **fork 的记录口径 = 记录前缀（append 顺序）**：子会话 = 源会话在 fork 点之前的**全部记录**（`_fork_slice`），uuid 全量重映射后写进子会话日志，再经加载路径构造内存态（"内存里就长得像重启后加载出来的样子"）。这样两件事同时成立：
  - **回得去**：被压缩区间、rewind 留在记录里的分叉都随行走（子会话的 `/fork` 候选与源会话一致，仍能回到压缩前的 User Message）；
  - **不上链**：活跃链由 tip 沿 `parent_uuid` 回溯自然得出——压缩节点是根（`parent_uuid=None` + `unzip_last_uuid` 指向区间末），已摘要内容不复活；选压缩**之前**的节点时压缩节点被切在前缀之外，子会话里压缩仿佛没发生过。
  没有链遍历（`walk_full_chain` / `trace_chain` 都退出 fork 路径）：活跃链是加载语义的**结果**，不是拷贝时要算的东西。
- **压缩节点的 `unzip_last_uuid` 是唯一编码**：任何复制 / 重链路径都必须把它带上——丢了，被压缩区间（乃至整段历史）就从 `/rewind`、`/fork` 候选里消失。已覆盖的两条路径：fork（记录前缀拷贝 + 全量重映射，见上）与 rewind（回退行复制 parent 时带上，回退到"压缩后第一条消息"不再塌候选）。
- 达到 `context_window_tokens` 触发压缩，保留 `keep_recent_tokens`；压缩由 `compactor.py` 的 LLM 摘要策略完成。

**KV cache 的已知边界（有意不处理）**：

- **冻结声明集不持久化**：热切换工具（`session/update` 的 `tools`）会把 `_declared_tools` 冻结在旧集合以保护**本进程**的前缀，但重启后 resume 直接采用当前可执行集，fork 子会话也以自己的可执行集重建声明——"热切换工具后又重启 / fork"会改变 `tools` 声明（前缀碎裂一次）。远程工具与动态工具切换尚无系统化设计（工具将整体迁出网关），按最简单语义处理：fork 记录子会话自己的有效快照，源侧降级（远程工具 ref 失效）后 fork 会把降级结果固化进子记录。
- **fork 无法复用父会话的缓存**：fork 产生新的 session id——`prompt_cache_key = session_id`（`explicit_cache_mode`，PR #17）意味着上游若按该 key 隔离缓存，父→子无法共享缓存块；且 `before_session_start` 会在子会话上重新注入（前缀本就会变）。这是设计取舍（fork = 新会话），不是待修 bug。

## 构建信息注入（commit hash）

`wing status` 报告的 gateway commit 来自**构建/安装时注入**，运行期不做任何 git 调用——发版 wheel 与开发环境 `pip install libs/core` 行为一致：

```
libs/core/hatch_build.py（hatchling custom build hook，构建/安装时执行）
  WING_COMMIT_HASH 环境变量（CI 传 github.sha，截短 7 位）
    → 构建期 git rev-parse HEAD（仅采信跟踪了包目录的仓库）
    → 保留旧值（sdist 构建）→ None
        │ 写入
  wing/_build_info.py（生成文件，gitignore；随 wheel 分发）
        │ 只读（导入时定型 = 进程启动快照）
  wing/build_info.py ──► /api/health（version + commit）──► wing status · wing start · 启动日志
```

- 生成文件内容随 commit/版本变化才重写（mtime 稳定）；解析不到 commit 时保留已注入的旧值（从 sdist 构建 wheel 不会把上游 commit 冲成 None）。
- 生成文件缺失时 `wing/build_info.py` 返回 None，health 回落到发行包元数据。
- commit 反映**上次安装/构建时**的 HEAD：改完代码要做集成测试前，重新安装并重启 gateway，`wing status` 才不会显示旧 commit。
  - `pip install libs/core`（in-tree 构建）会重跑钩子；uv workspace 里必须用 **`uv sync --reinstall-package wing-gateway`**——普通 `uv sync` 只看 `libs/core/pyproject.toml` 的 mtime 决定是否复用 editable 构建缓存，源码改了、提交了都不重跑钩子，生成文件停在上一次构建的 commit（实测）。
