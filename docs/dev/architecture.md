# 架构与数据流

## 三层结构

```
Frontends (wing 二进制)          Gateway (FastAPI)          Runtime (Python)
─────────────────────           ─────────────────          ─────────────────
TUI 模式（默认，ratatui）   ──►  GatewayServer          ──►  WingRuntime（协调者）
stdio 模式（wing -p，NDJSON）◄─  · routes/session/system     ├─ SessionManager（多会话）
                                 · routes/health/ws          ├─ SessionStore（持久化）
GatewayClient(WS) + ApiClient    · auth（opt-in）            ├─ ContextManager（压缩/回退）
                                                             └─ EventBus（事件路由）
```

- **Runtime** 是服务层协调者，本身不实现业务：路由 handler 只做参数校验 + 构造响应，逻辑下沉到 `Session` / `ContextManager`（PR #14）。
- **Gateway** 是 FastAPI 进程，持有 `EventBus` 订阅，把 Runtime 产生的事件经 WS 推给已订阅客户端。
- **Frontends** 都在 `wing` 二进制里，共享同一套 Gateway + Runtime。

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

## 两种前端

### TUI 模式（默认）

`wing`（无 `-p`）进入 ratatui 事件循环。核心是 `App` 状态机：

- 用户操作 / 命令 → `AppIntent`（意图，无副作用）→ `runner.rs` 执行（发起 HTTP/WS）；
- WS 事件 → `handle_event()` → 状态变更 → `draw()` 渲染。

斜杠命令大多被前端拦截转 HTTP（如 `/compact`→`POST /api/session/compact`）；`/clear`、`/copy` 纯前端本地处理；用户 `.md` prompt 命令展开为文本后作为普通消息发送（见 glossary「命令」）。

### stdio 模式（`wing -p`，PR #1）

无 human-in-the-loop 的头模式，**兼容 Claude Code 的 NDJSON 协议**——把 `wing` alias 为 `claude` 即可接入现有编排生态。

```bash
wing -p "列出文件"                          # text（默认）：仅输出最终结果
wing -p "列出文件" --output-format json      # json：单个 result 对象
wing -p "列出文件" --output-format stream-json  # 实时 NDJSON 流
```

`stream-json` 消息类型：`system/init`（tools/model/cwd）· `assistant`（content blocks + usage）· `user`（tool_result blocks）· `result`（终止信号：累计 usage / turns / 耗时）。支持 SDK 双向 stdin 握手（`--input-format stream-json`）。**未识别的 `--xxx` 参数被静默忽略**，确保外部编排层传递的 Claude 专有参数（如 `--permission-mode`）不报错。

> 在后台执行 `wing -p "request" > /tmp/result.md` 等价于调度了一个拥有任意命令执行权限的子 agent。多 agent 不易驾驭，yolo 本身危险，编排者应审慎使用。

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
| create | `POST /api/session/create` | 选 `backend: file\|memory` |
| resume | `POST /api/session/resume` | 还原 template_name + workspace |
| fork | `POST /api/session/fork` | 写完整 metadata（workspace/forked_from/template），继承源 backend；uuid 重映射在深拷贝上进行 |
| rewind | `POST /api/session/rewind` | 丢弃指定消息之后的内容 |
| compact | `POST /api/session/compact` | 委托 `ContextManager.do_manual_compact()` |
| interrupt | `POST /api/session/interrupt` | 委托 `Session.agent.interrupt()`；打断对账见下 |

### 中断提交语义

**不变量**：上下文中每个带 `tool_calls` 的 assistant 消息后必须跟齐每个 call_id 的 tool 消息——悬空 `tool_calls` 会被服务端拒绝，破坏 session。

面向现代模型的生成时序（reasoning → content → tool_calls，tool 流完响应才结束），打断分四种情况，其中两种需要对账：

- **流式三段**（reasoning / content / tool 参数流式）中打断：`_call_llm` 在 `except CancelledError` 中从 caller 持有的 accumulator 快照已累积的部分块——text/thinking 任意长度保留（半截无害），**未终结的 tool 块由 provider 剔除**（半截参数不可解析，且无配对结果会破坏不变量）——组装 partial assistant Message（`stop_reason="interrupted"`）提交进上下文，然后 re-raise。用户可放心打断长思考：已花费 tokens 的内容不丢，模型下轮请求能看到自己的半成品。
- **工具执行中**打断：LLM 响应已完整。`interrupt()`（async）cancel 旧 worker 并 **await 其拆卸完成**后重建 → 旧 worker 的 `exec_tool_calls` 在 `except CancelledError` 中收拢每个 call 的最终结果——已完成的取**真结果**，被取消的合成一句话结果（`"Tool call interrupted by user."`）——经 `_InterruptedToolResults` 抛给 `_llm_turn`，本轮消息沿**正常路径** `add_messages` 提交，然后重新抛出**原始** `CancelledError`（保留取消调用栈）让 worker 终止（不重抛则取消被消化，agent 会继续跑下一轮 LLM）。`shutdown()`（switch_template）走同一路径，免费获得同样保证。

**provider accumulator 协议**：`ModelProvider.generate(..., accumulator=)` 接受 caller 注入的不透明容器（`StreamAccumulator`），每次尝试（含重试）开始时填充新状态——取消后 caller 仍可经 `snapshot_blocks()` 读取已累积内容。`with_retry` 只捕 `Exception`（CancelledError 是 BaseException），取消直通，不会被重试吞掉（`test_with_retry.py` 钉死）。

**stop_reason 捕获**：两个 provider 均在最终 usage 携带协议原值（`end_turn`/`max_tokens`/`tool_use`/`stop`/`length`），传导进 `Message.stop_reason`（**唯一落盘审计位置**）与 `LLMCallMetricsEvent.stop_reason`（仅用于直播——该事件 persist=false，不落盘；metrics_registry 经 event_bus 聚合进独立的 metrics.json）。Anthropic 的 max_tokens 砍在 tool args 中间时，未终结的 tool 块从权威块数组剔除（半截 tool_use 不再被当作完整调用执行）；该剔除与未提交投影共用同一实现（`_ordered_finalized_blocks`）。

合成结果同时发射与正常完成相同的 `ToolCallResultEvent` / `ToolResultTurnEvent`：TUI 据此翻转 cell 状态（Bash 计时器仅在 cell 为 Pending 时前进，结果事件使其冻结——修复了打断后计时器不停的存量问题），stdio 模式据此输出 user turn 消息。

**时序**：runtime 先 await `agent.interrupt()`（补提交随之完成）再 emit `InterruptedEvent`（persist=true，落盘于 partial Message 之后，链序正确）——客户端观察到 Interrupted 时 store 已一致。收尸 gather 带 5s 兜底超时，行为不端的工具（吞掉取消）不会无限挂起补提交路径。

## 事件系统与统一日志

**事件是唯一事实来源**：history.jsonl 是混合日志，承载两种记录——Message 记录（`role ∈ user/assistant/tool/system`，进入 LLM 上下文）与事件记录（`role="event"`，携带事件自身字段）。所有记录共享链拓扑（uuid/parent_uuid），事件参与链构建；`Message` 与前端 `ChatView` 都是同一日志的投影。记录级判别只用 `role`。

**未提交内容的单一权威 = provider accumulator**：turn 进行中"已生成但未提交"的内容只有一处权威——caller（`ReActLoop`）持有的流累积状态（`_current_acc`，一轮 LLM 调用生命周期：`_call_llm` 入口新建、轮提交/中断补提交/turn 收口后置空）。对外两个覆盖互斥的投影，按需快照、不缓存副本：

- `snapshot_blocks(acc)`：**已终结**块（text/thinking 任意长度保留，未终结 tool 块剔除）——中断补提交与未提交 Message 投影**同源**（resume 与打断变成同一个操作）。
- `pending_tool_calls(acc)`：**未终结** tool 调用的原始 args 文本（`PendingToolView`）——活工具卡渲染素材，后端不解析半截 JSON（局部解析在客户端 `partial_json.rs`）。

**落盘只存事实，不存副本**：`persist=true` 事件（diff/ask/interrupted/error/compact_done）在完整产生时即时落盘进链；流式 delta（`persist=false`）纯广播——不落盘、不进任何内存缓冲，其内容由轮提交时的 Message 记录承载。`tool_call_result` / `llm_call_metrics` 是 Message 孪生（`role="tool"` Message / `Message.usage` + `Message.stop_reason`），已停止落盘（事件本身保留：metrics_registry 与 TUI 直播经 event_bus 依赖）；`turn_result` 保留落盘但 `result` 字段（最终文本孪生）经 `disk_exclude` 排除出磁盘记录（wire 帧仍携带，stdio 消费）。

**persist 分流原则**：`WingEvent.persist` 是 `ClassVar[bool]`（非 pydantic 字段——旧的 `Field(exclude=True)` 会被子类重声明静默击穿），基类默认 true。判据两条同时成立：**是事实**（读回来仍成立，非一次性信号）**且无 Message 孪生**。false 仅限（a）流式 delta，（b）与 Message 完全孪生且体积可观的事件（AssistantTurn/ToolResultTurn/ToolCall/ToolCallResult/LLMCallMetrics），（c）可从权威状态实时重建的协议/查询事件（Sync/SessionInit/ContextStats/BranchTargets/Delivered/SessionStateChanged）。

**三个序列化边界，三套剥离规则**（不可用一个 `model_dump(exclude_none)` 打通——`Message._serialize_flat` 是 wrap 序列化器，`content`/`reasoning_content`/`tool_calls` 在内层 handler 之后才注入）：磁盘记录（`TrackedList._to_record`：字典推导剥 null + 剥 target + disk_exclude）、WS 直播帧与 SyncSession 载荷（`event/__init__.py::wire_dump`：剥 null + 剥 `parent_uuid`/`unzip_last_uuid`/`role`/`target`，保留 `uuid`）。`serialize_event` 是 `wire_dump` 的别名——一套规则，无分散实现。

**rewind/fork/compact 凭链序免费工作**：事件是链节点——set_tip 后边界外事件移出活跃链；fork 拷贝混合链前缀（事件随行）；compact 后被压缩区间的事件与消息一并离开活跃链（被压缩的 diff 自然不再重放，无孤儿处理）。

**中途订阅完整视图**：`_push_sync` 的 SyncSessionEvent 携带四组重放素材——`messages`（已提交 Message 投影）+ `uncommitted`（**单个**未提交 assistant Message 投影，已终结块）+ `uncommitted_tools`（未终结调用的原始 args 片段）+ `events`（活跃链上的**事实类**事件，按链序），外加 `turn_started_at`（恢复 working 已耗时）。前端组装顺序 **messages → uncommitted → uncommitted_tools → events → live 流**：`messages` 与 `uncommitted` 走同一条 `replay_messages` 路径，`uncommitted_tools` 走既有 live `ToolCallStream` 分支（客户端局部解析），`events` 最后锚定。这个顺序让"diff 渲染在其 ToolCall 卡片之前"的缺陷结构性消失——产生 diff 的 tool_use 是已终结块，必在 `uncommitted` 投影里先建出锚点 cell。`events` 的下发过滤归属后端单点（`FACT_EVENTS` + pending ask 谓词，见下）。

**pending ask 重放**：`AskEvent` 落盘，但已答的 ask 不再是关于当下的事实（重放会渲染活的 Ask 卡，回答找不到 waiter 进虚空）。后端用 `Inbox._feedback_waiters` 的键集合（`pending_ask_ids()`）作权威待答集合，`get_active_events()` 据此只下发仍挂起的 ask；前端重放渲染 Ask cell 并注册 reply flow（`AskFlow`/`AskSelection`），用户回答走既有通道 resolve waiter。

**前端重放**：`replay.rs` 两段式——先 replay_messages（建 ToolCall/thinking/text cell），再 replay_events（**能力分发**：diff 按 tool_call_id 锚定插入对应 ToolCall cell 后、ask 渲染为可答卡片，无渲染器的类型跳过以保持前向容忍；未知锚点 fallback append，与直播路径同款兜底）。前端**不再编码"孪生事件不得渲染"这类策略**——过滤已在后端 `FACT_EVENTS` 完成，前端只做能力分发。

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
- 达到 `context_window_tokens` 触发压缩，保留 `keep_recent_tokens`；压缩由 `compactor.py` 的 LLM 摘要策略完成。
