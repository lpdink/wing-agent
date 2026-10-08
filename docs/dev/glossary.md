# 核心概念速查

按主题分组，每条尽量一句话讲清「是什么 / 在哪」。

## 运行时

| 概念 | 说明 |
|------|------|
| **WingRuntime** | 服务层协调者（`runtime.py`）。路由 handler 薄化，逻辑下沉到 Session / ContextManager。 |
| **Session** | 一个会话：消息链 + 状态 + metadata，经 SessionStore 持久化（`session/session.py`）。 |
| **SessionManager** | 多会话管理 + fork/resume + store 注册表（`{name: store}`）（`session/manager.py`）。 |
| **AgentTemplate** | agent 模板：model（引用 id）/ tools / system_prompt / skills / rules，来自配置 `agents:`；`model` 是 id，经 `Config.find_model()` 查表得到调用名与 provider（`session/template.py`，无「默认第一个 provider」）。 |
| **WingAgent** | ReAct agent，`wing/agent/` 包（core / react_loop / tool_executor / event_sink / inbox / tool_context）；公开导入路径经 re-export 保持不变（PR #53）。 |
| **ToolContext** | 工具侧窄接口 Protocol（session_id / yolo / cwd / ask_feedback / emit / interrupt hooks）；工具收 `ctx` 而非整个 agent，取代旧的 `AgentStateBag` 字符串耦合（PR #53）。 |
| **EventBus** | 全局单例事件路由，Runtime 发事件、Gateway 订阅转发（`event_bus.py`）。 |

## 模型（三名词）

「引用词」只有一个：**model_id**。provider 与调用名都是运行期事实，不再是引用维度——
一切请求 / 协议 / CLI / metadata 引用 model_id，解析 = **单键查表**（`Config.find_model()`）。

| 名词 | 定义 | 谁用 |
|------|------|------|
| **model_id** | 全局唯一**引用词**。配置声明时可选（缺省 = `name`，存量字符串形态零改动）；`providers[].models[].id` 的投影 | 协议、配置引用（`agents[].model`）、metadata、前端选择态 |
| **name** | 上游**调用名**（发给 provider API 的值） | 运行期、审计 |
| **display_name** | **展示名**（缺省回落 name；未声明 = null） | 渲染层（状态栏 / 模型面板），不参与匹配 |
| provider | 运行期**事实**维度 + 展示分组（**不再是引用词**） | `AgentInfo` / `/api/models` 分组 / 状态事件 |

不变量（写进代码注释与测试）：

1. **id 全局唯一**：配置加载期强制（跨 provider）；`agents[].model` 必须是 id（查不中即加载失败，
   错误含 available ids + 调用名提示）。运行期只有「命中 / 未命中」二值判断——**无候选集合、
   无优先级、无回落**。
2. **同 provider 内 name 唯一**（现状保留）：`(provider, name) → 声明项` 反查（`Config.identify()`）
   无歧义；该反查只用于旧数据迁移 / 补 id，**绝不出现在请求解析路径**。
3. **解析 = 单键查表**：`id → (provider, name)` 由配置唯一决定；重命名 / 重排 / 顺序不影响结果。
   任何「基于 id 字符串结构的解析」（`:` 前缀等）永久禁止。
4. **目录 = 配置静态声明的同步投影**：远端 `GET /models` 发现已退役，`/api/models` 是对外可依赖
   的合同（每个 id 全局唯一）。

恢复链（resume 时决定跑在哪个模型上）：`metadata.model_id` 命中 → 用**当前映射**
（`ref.name` / `ref.provider_name`，跟随配置演化）；未命中（id 被删）→ 用记录里的快照
`(provider_name, model_name)` 继续跑 + warning，`identify` 反查补 id；快照 provider 也不可解析
→ 回落模板默认 + warning（记录保留，config 修好后仍可还原）；旧记录（无 id）走快照路径 + 反查补 id。
restore 后内存态与记录不一致才落盘对齐（一次性迁移，非写噪声）。

## 持久化（PR #39）

| 概念 | 说明 |
|------|------|
| **SessionStore** | 会话持久化的**唯一**所有者（ABC，`store/base.py`）。backend：`file` / `memory`，建会话时选。 |
| **MessageLog** | 追加式混合记录 + aux kv（`store/base.py`）。pending compaction 存于 aux。newest.json 快照已移除（重放由混合日志承担）。 |
| **TrackedList** | 纯内存链拓扑引擎（uuid/parentUuid），ChainNode 家族混排（Message + 事件节点），I/O 全委托 MessageLog（`chain.py`）。 |
| **SessionMetadata** | 会话元数据模型（workspace、forked_from、template_name、model_id/provider_name/model_name、last_interaction…）。 |
| **模型绑定持久化** | 会话当前模型的**三元组** `(model_id, provider_name, model_name)`：`model_id` 是引用词，`provider_name` / `model_name` 是当时的运行期事实快照（`session/session.py::_persist_model`）。写入时机是**显式动作**（模型切换、模板切换、创建 override、fork 快照；未动过模型的 session 不写）。resume 按上面「恢复链」还原（id 优先 → 快照兜底 → 模板默认）；进程存活期间前端渲染与后端使用同源于 agent，本机制解决的是重启后的还原。 |

## 事件系统

| 概念 | 说明 |
|------|------|
| **混合日志** | history.jsonl 单一事实来源：Message 记录（role ∈ user/assistant/tool/system，进 LLM 上下文）+ 事件记录（`role="event"`，含事件自身字段），共享链拓扑。记录级判别只用 role。 |
| **未提交投影（uncommitted）** | turn 进行中"已生成未提交"内容的唯一权威 = caller 持有的 provider accumulator（`ReActLoop._current_acc`，一轮生命周期）。两个覆盖互斥的投影按需快照：`snapshot_blocks()`（已终结块——中断补提交与 resume 同源）+ `pending_tool_calls()`（未终结调用原始 args，后端不解析）。取代旧的内存事件缓冲。 |
| **落盘只存事实** | `persist=true` 事实事件（diff/ask/interrupted/error/compact_done）即时落盘；流式 delta（persist=false）纯广播、不落盘不缓冲（内容由轮提交的 Message 承载）。孪生事件（tool_call_result/llm_call_metrics）停止落盘；turn_result 落盘但 result 字段经 disk_exclude 排除。 |
| **persist 分流** | `WingEvent.persist` 是 `ClassVar[bool]`（非 pydantic 字段——旧 `Field(exclude=True)` 会被子类重声明击穿），基类默认 true。判据：**是事实** 且 **无 Message 孪生**。false 仅限流式 delta、Message 孪生体积事件、可实时重建的协议/查询事件。 |
| **中断补提交** | 流式期间打断：accumulator 快照部分块（text/thinking 保留、未终结 tool 块剔除）→ partial Message（`stop_reason="interrupted"`）提交 → re-raise，当前 accumulator 置空。用户可放心打断长思考。 |
| **accumulator 协议** | `generate(..., accumulator=)` caller 注入容器，provider 每次尝试填充新状态；取消后 `snapshot_blocks()` 取已终结块、`pending_tool_calls()` 取未终结调用。 |
| **stop_reason** | provider 捕获协议原值（end_turn/max_tokens/tool_use/stop/length）→ **`Message.stop_reason` 是唯一落盘审计位置**；LLMCallMetricsEvent.stop_reason 仅用于直播（persist=false）。 |
| **事件链锚定** | rewind/fork/compact 凭链序免费工作：事件是链节点，set_tip/fork 拷贝/压缩边界自然裁剪事件可见性。 |
| **中途订阅视图** | SyncSessionEvent = 状态 + 四组素材：status（快照时刻的 idle/working/waiting，**必填、权威**——working 不可由内容反推，也没有回落推断）+ turn_started_at（恢复已耗时）+ messages（已提交投影）+ uncommitted（单个未提交 assistant Message 投影）+ uncommitted_tools（未终结调用原始 args）+ events（活跃链**事实**事件）。前端按 **messages → uncommitted → uncommitted_tools → events → live** 组装（uncommitted 走 replay_messages、uncommitted_tools 走 live ToolCallStream 分支），diff 锚点结构性先于 diff 存在。 |
| **事实事件下发过滤** | 后端单点策略：`get_active_events()` 按 `FACT_EVENTS`（与 persist 标记同处 `event/__init__.py`）过滤，ask 额外按 `pending_ask_ids()`（inbox feedback waiters）过滤。前端只做能力分发（有渲染器则渲染），不编码"孪生不得渲染"策略。存量孪生记录加载进链但不下发（零迁移）。 |
| **diff 载荷窗口** | `DiffContentEvent` 的 old_text/new_text 只带变更区域 ± 3 行（`tools/internal/diff_window.py`）与窗口首行绝对行号（old_start_line/new_start_line，缺失按 1）。Write / 新建文件仍全量；`replace_all` 每匹配一条事件（同一 tool_call_id）。前端逐行渲染、不折叠（`LayoutConfig.diff_context` 已删）。 |

## 上下文与压缩

| 概念 | 说明 |
|------|------|
| **ContextManager** | 上下文窗口跟踪 + 压缩 + 回退（`context/manager.py`）。 |
| **Compactor** | 压缩策略（LLM 摘要）（`context/compaction.py`）。 |
| **缓存前缀** | 核心哲学：除压缩外绝不破坏 prompt 缓存前缀，追求理论最高命中率。**前缀身份 = 会话状态**：system 段（含 append_system_prompt）、tools 声明、处理开关都算，全部随会话持久化。 |
| **append_system_prompt** | 追加系统提示词：`before_session_start` hook 注入（如 workspace / OS 信息）+ `AgentOverride.append_system_prompt`（CLI `--append-system-prompt`）的合并结果；持久化在 `metadata.json`，resume 时逐字节还原（不触发 hook）。fork 属新会话：子会话先继承该值、hook 再注入一次。 |
| **持久会话状态** | `metadata.json` 记录的会话级状态：模型绑定、`system_prompt` / `append_system_prompt`、`tools` 覆盖、`thinking` / `reasoning_effort` / `yolo` / `max_turns`。显式动作写入、resume 优先于模板/配置、fork 按 fork 时刻的有效值一次写全（快照；`thinking` / `reasoning_effort` 只拷显式记录，不固化 provider 派生默认）。 |
| **before_session_start 触发条件** | 只看 **session id 是否变化**：create / fork（新 id）触发，resume（同一 id 换入内存）不触发。 |
| **fork 冻结派生值** | fork 把当时的**live 有效值**写进子会话记录（`system_prompt` / `tools` / `yolo` / `max_turns`）——它们可能来自模板/配置：子会话因此不随后续模板/配置变更漂移（源会话会）。这是快照语义的代价，刻意如此。 |
| **fork 记录前缀** | fork = 拷贝源会话在 fork 点之前的**全部记录**（append 顺序）+ uuid 全量重映射，子会话经加载路径构造；活跃链由 tip 回溯得出（压缩节点即止）——被压缩区间随行（回得去）但不活跃（不复活）。目标消息自身不进拷贝：它由响应里的 `draft` 重新发送。 |
| **reasoning_effort** | 推理强度 `low/medium/high/xhigh/max`，经 extra_body 发送；`/think` 命令可切换。 |

## 工具（PR #34）

| 概念 | 说明 |
|------|------|
| **Tool** | 工具模型（`schema/tool.py`），含 `namespace`、`llm_name`、`effective_llm_name`、`to_openai()`。 |
| **ToolRegistry** | 命名空间感知注册表：`dict[namespace → {name → Tool}]`（`tool_registry.py`）。 |
| **ToolRef** | 字符串引用解析：`"Bash"`→`(default, Bash)`，`"client.Bash"`→`(client, Bash)`（k8s 风格 `rsplit(".", 1)`）。 |
| **namespace** | 按来源分组工具，让多来源同名工具（如多个远程 `Bash`）共存。内置工具在 `default`。 |
| **llm_name** | LLM 可见名（function calling schema 里的名字）；缺省等于注册名 `name`。 |
| **并发执行** | 工具调用经 `asyncio.gather` 并发（PR #33）；结果按 `tool_call_id` 回填（PR #37）。 |
| **结果截断** | 超长工具结果头尾保留 + 全文落临时文件（PR #19，`tool_result_truncate`）。 |
| **远程工具** | tool host 经 HTTP 注册（`POST /api/tools/register`）+ WS 服务调用（`tool_call_request` / `tool_call_result`）；核心网络无关，远程工具即普通 `Tool`（gateway 注入 dispatch 闭包）（PR #47，`gateway/remote_tools.py`）。 |
| **client_id** | tool host 在 WS 连接经 `?client_id=<id>` 自选，既是工具 namespace 也是身份，与 RBAC 角色解耦；`default` 为保留字（内置命名空间）。 |
| **动态工具切换** | 运行期经 `POST /api/session/update`（`tools`，全量替换）换工具集；ContextManager 拥有 LLM 可见的 declared 视图 + 冷/热冻结策略以保护 KV cache（PR #50）。 |
| **流式渲染** | LLM 生成参数期间发 `tool_call_stream`（增量 `args_fragment`），后端不解析 partial JSON，前端累积并容错解析渲染（`util/partial_json.rs`）（PR #43/#45）。 |

## 命令

wing 的斜杠命令分两类（「magic command dispatch」已在 PR #14 移除，`commands.py`（原 `magic_command/`）现仅存元数据 + 文本展开）：

| 类别 | 机制 | 例子 |
|------|------|------|
| **前端命令** | TUI 拦截，转 HTTP 调用或本地处理 | `/compact`→HTTP、`/model`→HTTP、`/clear`·`/copy`→本地 |
| **prompt 命令** | 用户 `.md` 文件，`$ARGUMENTS` 文本展开后作为普通消息发送（`commands.py`） | 用户自定义 `/plan` 等 |

命令清单的真相在 `crates/wing/src/ui/popup/command.rs`（`TUI_ONLY_COMMANDS`）；`GET /api/commands` 仅返回 prompt 命令。

## 前端能力

| 概念 | 说明 |
|------|------|
| **stdio 模式** | `wing -p` 头模式，Claude 协议 NDJSON，text/json/stream-json 三种输出（PR #1）。 |
| **SyncSession** | 订阅时重放历史消息的事件（`event/state_change.py`）。 |

## 扩展与网关

| 概念 | 说明 |
|------|------|
| **Hooks** | 扩展点：`before_session_start` / `before_user_message` / `before_tool_call` / `after_tool_call`（`hooks/`）。官方包 `wing-hooks`。 |
| **Gateway 鉴权** | opt-in API key（HTTP header / WS query），`/api/health` 豁免；TLS 交给反代（PR #35）。 |
| **RBAC 角色** | `admin`（全量）/ `tool_runtime`（纯工具执行远端，仅注册端点 + 工具 WS，不收事件）；`role` 字段现已强制（PR #47）。 |
| **wing-sdk** | Python 远程工具宿主 SDK：decorator 注册 + WS serve loop + 标准工具（`libs/wing-sdk/`，PR #49）。Rust 对应 `wing-api-client::tool_host`。 |
| **HTTP 生命周期** | `wing start/stop/status` 全基于 `/api/health` + `/api/shutdown`，无 PID / state.json（PR #10）。 |
| **yolo** | 跳过危险命令审查（agent 级设置）。 |
| **steer** | 以 steering prompt 引导 agent 行为。 |
