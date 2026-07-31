# 核心概念速查

按主题分组，每条尽量一句话讲清「是什么 / 在哪」。

## 运行时

| 概念 | 说明 |
|------|------|
| **WingRuntime** | 服务层协调者（`runtime.py`）。路由 handler 薄化，逻辑下沉到 Session / ContextManager。 |
| **Session** | 一个会话：消息链 + 状态 + metadata，经 SessionStore 持久化（`session.py`）。 |
| **SessionManager** | 多会话管理 + fork/resume + store 注册表（`{name: store}`）（`session_manager.py`）。 |
| **AgentTemplate** | agent 模板：model / tools / system_prompt / skills / rules，来自配置 `agents:`（`agent_template.py`）。 |
| **WingAgent** | ReAct agent，`wing/agent/` 包（core / react_loop / llm_caller / tool_executor / event_sink / inbox / tool_context）；公开导入路径经 re-export 保持不变（PR #53）。 |
| **ToolContext** | 工具侧窄接口 Protocol（session_id / yolo / cwd / ask_feedback / emit / interrupt hooks）；工具收 `ctx` 而非整个 agent，取代旧的 `AgentStateBag` 字符串耦合（PR #53）。 |
| **EventBus** | 全局单例事件路由，Runtime 发事件、Gateway 订阅转发（`event_bus.py`）。 |

## 持久化（PR #39）

| 概念 | 说明 |
|------|------|
| **SessionStore** | 会话持久化的**唯一**所有者（ABC，`store/base.py`）。backend：`file` / `memory`，建会话时选。 |
| **MessageLog** | 追加式消息持久化 + 快照 + aux kv（`store/base.py`）。pending compaction 存于 aux。 |
| **TrackedList** | 纯内存链拓扑引擎（uuid/parentUuid），I/O 全委托 MessageLog（`common/tracked_list.py`）。 |
| **SessionMetadata** | 会话元数据模型（workspace、forked_from、template_name、last_interaction…）。 |

## 上下文与压缩

| 概念 | 说明 |
|------|------|
| **ContextManager** | 上下文窗口跟踪 + 压缩 + 回退（`context_manager.py`）。 |
| **Compactor** | 压缩策略（LLM 摘要）（`compactor.py`）。 |
| **缓存前缀** | 核心哲学：除压缩外绝不破坏 prompt 缓存前缀，追求理论最高命中率。 |
| **reasoning_effort** | 推理强度 `low/medium/high/xhigh/max`，经 extra_body 发送；`/think` 命令可切换。 |

## 工具（PR #34）

| 概念 | 说明 |
|------|------|
| **Tool** | 工具模型（`schema.py`），含 `namespace`、`llm_name`、`effective_llm_name`、`to_openai()`。 |
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

wing 的斜杠命令分三类（「magic command dispatch」已在 PR #14 移除，`magic_command/` 现仅存元数据 + 文本展开）：

| 类别 | 机制 | 例子 |
|------|------|------|
| **前端命令** | TUI 拦截，转 HTTP 调用或本地处理 | `/compact`→HTTP、`/model`→HTTP、`/clear`·`/copy`→本地 |
| **prompt 命令** | 用户 `.md` 文件，`$ARGUMENTS` 文本展开后作为普通消息发送（`magic_command/prompt_commands.py`） | 用户自定义 `/plan` 等 |
| **Goal 命令** | TUI 编排状态机 | `/goal`、`/goal-exit` |

命令清单的真相在 `crates/wing/src/ui/popup/command.rs`（`TUI_ONLY_COMMANDS`）；`GET /api/commands` 仅返回 prompt 命令。

## 前端能力

| 概念 | 说明 |
|------|------|
| **Goal 模式** | executor/checker 验证循环，`goal.rs` 纯状态机（PR #22）；TUI 内 `/goal` 前台运行，`wing-orch` 提供后台 + 持久化 + resume 版本（PR #49）。 |
| **stdio 模式** | `wing -p` 头模式，Claude 协议 NDJSON，text/json/stream-json 三种输出（PR #1）。 |
| **SyncSession** | 订阅时重放历史消息的事件（`event/state_change.py`）。 |

## 扩展与网关

| 概念 | 说明 |
|------|------|
| **Hooks** | 扩展点：`before_session_start` / `before_user_message` / `before_tool_call` / `after_tool_call`（`hook_registry.py`）。官方包 `wing-hooks`。 |
| **Gateway 鉴权** | opt-in API key（HTTP header / WS query），`/api/health` 豁免；TLS 交给反代（PR #35）。 |
| **RBAC 角色** | `admin`（全量）/ `tool_runtime`（纯工具执行远端，仅注册端点 + 工具 WS，不收事件）；`role` 字段现已强制（PR #47）。 |
| **wing-sdk** | Python 远程工具宿主 SDK：decorator 注册 + WS serve loop + 标准工具（`libs/wing-sdk/`，PR #49）。Rust 对应 `wing-api-client::tool_host`。 |
| **wing-orch** | 后台 Goal 编排 CLI：executor/checker 循环、原子状态持久化 + resume，依赖 wing-sdk（`libs/wing-orch/`，PR #49）。 |
| **HTTP 生命周期** | `wing start/stop/status` 全基于 `/api/health` + `/api/shutdown`，无 PID / state.json（PR #10）。 |
| **yolo** | 跳过危险命令审查（agent 级设置）。 |
| **steer** | 以 steering prompt 引导 agent 行为。 |
