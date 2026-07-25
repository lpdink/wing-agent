# 核心概念速查

按主题分组，每条尽量一句话讲清「是什么 / 在哪」。

## 运行时

| 概念 | 说明 |
|------|------|
| **WingRuntime** | 服务层协调者（`runtime.py`）。路由 handler 薄化，逻辑下沉到 Session / ContextManager。 |
| **Session** | 一个会话：消息链 + 状态 + metadata，经 SessionStore 持久化（`session.py`）。 |
| **SessionManager** | 多会话管理 + fork/resume + store 注册表（`{name: store}`）（`session_manager.py`）。 |
| **AgentTemplate** | agent 模板：model / tools / system_prompt / skills / rules，来自配置 `agents:`（`agent_template.py`）。 |
| **AgentStateBag** | 每个 agent 的可变运行状态（yolo、reasoning effort 等）（`agent_state_bag.py`）。 |
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
| **Goal 模式** | TUI-side executor/checker 验证循环，`goal.rs` 纯状态机（PR #22）。 |
| **stdio 模式** | `wing -p` 头模式，Claude 协议 NDJSON，text/json/stream-json 三种输出（PR #1）。 |
| **SyncSession** | 订阅时重放历史消息的事件（`event/state_change.py`）。 |

## 扩展与网关

| 概念 | 说明 |
|------|------|
| **Hooks** | 扩展点：`before_session_start` / `before_user_message` / `before_tool_call` / `after_tool_call`（`hook_registry.py`）。官方包 `wing-hooks`。 |
| **Gateway 鉴权** | opt-in API key（HTTP header / WS query），`/api/health` 豁免；TLS 交给反代（PR #35）。 |
| **HTTP 生命周期** | `wing start/stop/status` 全基于 `/api/health` + `/api/shutdown`，无 PID / state.json（PR #10）。 |
| **yolo** | 跳过危险命令审查（agent 级设置）。 |
| **steer** | 以 steering prompt 引导 agent 行为。 |
