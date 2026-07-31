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

面向现代模型的生成时序（reasoning → content → tool_calls，tool 流完响应才结束），打断分四种情况，只有一种需要对账：

- **流式三段**（reasoning / content / tool 参数流式）中打断：LLM 响应未完成，半截响应丢弃，上下文不变。
- **工具执行中**打断：LLM 响应已完整。`interrupt()`（async）cancel 旧 worker 并 **await 其拆卸完成**后重建 → 旧 worker 的 `exec_tool_calls` 在 `except CancelledError` 中收拢每个 call 的最终结果——已完成的取**真结果**，被取消的合成一句话结果（`"Tool call interrupted by user."`）——经 `_InterruptedToolResults` 抛给 `_llm_turn`，本轮消息沿**正常路径** `add_messages` 提交，然后重新抛出**原始** `CancelledError`（保留取消调用栈）让 worker 终止（不重抛则取消被消化，agent 会继续跑下一轮 LLM）。`shutdown()`（switch_template）走同一路径，免费获得同样保证。

合成结果同时发射与正常完成相同的 `ToolCallResultEvent` / `ToolResultTurnEvent`：TUI 据此翻转 cell 状态（Bash 计时器仅在 cell 为 Pending 时前进，结果事件使其冻结——修复了打断后计时器不停的存量问题），stdio 模式据此输出 user turn 消息。

**时序**：runtime 先 await `agent.interrupt()`（补提交随之完成）再 emit `InterruptedEvent`——客户端观察到 Interrupted 时 store 已一致。收尸 gather 带 5s 兜底超时，行为不端的工具（吞掉取消）不会无限挂起补提交路径。剩余取舍仅展示层：半截流式内容前端已渲染但未入库，replay 时自然对齐。

## 持久化（PR #39）

`SessionStore` 是会话**唯一**的持久化所有者（metadata + message log + aux）。历史上四个独立写者导致 fork 丢 workspace 等结构性 bug，现统一收口。

```
Session / SessionManager
        │
SessionStore (ABC) ── load/save_metadata · open_log → MessageLog · list/resolve
  ├── FileSessionStore     (~/.wing/core/sessions/，布局不变，零迁移)
  └── MemorySessionStore   (进程内，不落盘)
  └── [未来: Sqlite / Pg / Supabase / Redis —— 增量实现，非架构改动]

TrackedList = 纯内存链拓扑引擎（uuid/parentUuid、trace、find、set_tip），
              所有 I/O 委托 MessageLog；log=None 即纯内存
MessageLog  = 追加式记录 + 快照 + aux kv（pending compaction 存于此）
```

接口与存储无关：`SessionStore` 6 方法 + `MessageLog` 6 方法，全是 kv / append-only / listing 语义，不泄漏路径 / fsync / glob。一个 PG 后端就是三张表（sessions / messages / aux）。

## 压缩与缓存哲学

- **不在上下文中施加魔法**：从不注入隐藏 system prompt。
- **理论最高缓存命中率**：除压缩外绝不破坏缓存前缀；`explicit_cache_mode` 可为支持的 provider（如 DashScope）追加 `cache_control` 标记（PR #17）。
- 达到 `context_window_tokens` 触发压缩，保留 `keep_recent_tokens`；压缩由 `compactor.py` 的 LLM 摘要策略完成。
