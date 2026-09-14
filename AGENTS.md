# wing-agent

Monorepo：Python agent runtime（`libs/core/wing/`，pip 包 `wing-gateway`）+ Rust 前端（`crates/wing/`，一个 `wing` 二进制，提供 TUI / stdio / 编排 CLI 三种形态）。

> **维护约定**：本文件只保留高信息密度总览。**不要随手往里加东西，除非 user 明确要求或同意**；机制细节、临时知识、踩坑记录一律写进 `docs/dev/`（见文末「深潜阅读」登记表）。

## 架构

```
┌─────────────────────────────┐             ┌─────────────────────────┐               ┌─────────────────────────┐
│ Frontends（wing 二进制）    │             │ Gateway (FastAPI)       │               │ Runtime (Python)        │
│ TUI（默认）· ratatui 循环   │──── WS ────►│ GatewayServer           │──── HTTP ────►│ WingRuntime（协调者）   │
│ stdio（wing -p）· NDJSON    │             │ · routes/session(14)    │               │ ├ SessionManager        │
│ 编排 CLI · run/wait/ps/…    │◄── 事件 ────│ · routes/system(6)      │◄──────────────│ ├ SessionStore          │
│ Goal loop（TUI 侧）         │             │ · routes/tools · health │               │ ├ ContextManager        │
│ GatewayClient(WS)+ApiClient │             │ · routes/ws（事件流）   │               │ ├ EventBus              │
│ HTTP 建会话 → WS 订阅       │             │ auth（opt-in）          │               │ └ provider/（LLM 调用） │
└─────────────────────────────┘             └─────────────────────────┘               └─────────────────────────┘
```

- **三种前端形态，同一个二进制**：TUI（默认，human-in-the-loop）；stdio（`wing -p`，headless，Claude Code 兼容 NDJSON——把 `wing` alias 为 `claude` 即可接入外部编排器）；编排 CLI（`wing run/wait/ps/info/tail/head` 后台任务，`wing start/stop/status` 网关生命周期）。
- **协议**：HTTP 承载生命周期 / 查询 / 变更（22 个 RPC 端点）；WebSocket（`/ws`）只承载实时 ReAct 事件流 + 客户端上行帧（message / Ask 回答 / tool_call_result）。会话创建与 WS 握手解耦：先 HTTP 建会话，再订阅事件。API key 鉴权在网关 opt-in（HTTP header / WS query param），TLS 交给反向代理。
- **持久化**：`SessionStore` 是会话全部持久状态（metadata、混合 message/event 日志、aux）的唯一所有者；后端 `file`（默认，`~/.wing/core/sessions/`）与 `memory`（进程内）。`TrackedList` 是纯内存链拓扑引擎（uuid/parentUuid），I/O 全部委托 `MessageLog`；SQL 后端是增量实现，非架构改动。
- **模型调用**：`provider/` 隔离协议差异（OpenAI 兼容 / Anthropic），ReAct 循环对协议无感知。

**改代码前必读的不变量**（细节一律在 docs/dev，不要在这里展开）：

1. `history.jsonl` 是唯一事实来源：Message 记录（role ∈ user/assistant/tool/system）与事件记录（`role="event"`）混排、共享链拓扑；落盘只存事实不存副本，流式 delta（`persist=false`）永不落盘。
2. turn 进行中「已生成未提交」内容的唯一权威是 provider 流累积器（`ReActLoop._current_acc`）：按需投影（`snapshot_blocks` / `pending_tool_calls`），后端从不解析半截 JSON（局部解析在前端 `util/partial_json.rs`）。
3. 中断后上下文必须自洽：每个带 `tool_calls` 的 assistant 消息必须跟齐每个 call_id 的 tool 消息；未终结（半截参数）的 tool 块一律剔除。
4. 工具不必跑在 gateway 进程内：远程工具 = 普通 `Tool` + 注入的 dispatch 闭包（`gateway/remote_tools.py`），核心保持网络无关。
5. 运行期改工具集（`POST /api/session/update`）需 KV cache 保护：链空冷切换；链非空冻结 declared 视图 + 注入 System Reminder（策略归 `ContextManager`）。
6. 任何出网 WS 帧 ≤ 16 MiB（= 客户端默认上限）：超 8 MiB 的载荷由 gateway 在 wire 出口切分（`_chunk` 信封）、客户端在读任务内合并还原，应用层只见完整事件。

## 项目结构

> 目录树是代码结构的索引：新增 / 删除 / 重命名模块时请同步更新本节。

### 后端：`libs/core/wing/`（Python runtime）

```
libs/core/wing/
├── __init__.py / _version.py        包入口（re-export execute_shell，触发 metrics 订阅）/ 版本号
├── build_info.py                    构建信息读取口：版本 + commit hash（构建时注入，运行期零 git）
├── runtime.py                       WingRuntime — service 层协调者（post() 唯一入站，路由到 Session/CM）
├── session.py                       Session — messages + state + metadata（经 SessionStore）
├── session_manager.py               SessionManager — 多会话、fork/resume、store registry
├── context_manager.py               上下文窗口跟踪 + 压缩 + rewind
├── compactor.py                     压缩策略（LLM 摘要）
├── agent_template.py                AgentTemplate — 配置 agents: 的 model/tools/prompt/skills/rules
├── config.py                        Config 模型 + WING_HOME 解析
├── default_config.py                手写默认 config.yaml 模板（事实来源）
├── schema.py                        Tool / ToolParam / Message 等核心 schema
├── tool_registry.py                 ToolRegistry — 命名空间感知注册表 + ToolRef 解析
├── event_bus.py                     EventBus — 全局单例事件路由
├── hook_registry.py                 Hook 扩展点（before_session_start / before_user_message / before_tool_call / after_tool_call）
├── request_context.py               每请求上下文（request_id / session_id / client_id，单 ContextVar）
├── agent/                           WingAgent 包（公共 API 经 __init__ re-export，导入路径不变）
│   ├── core.py                      瘦壳：组装、公共 API、worker 生命周期、未提交投影
│   ├── react_loop.py                ReAct 主循环：drain → hook → LLM → tools → commit
│   ├── tool_executor.py             工具并发分发（asyncio.gather）+ 中断拆卸
│   ├── event_sink.py                AgentEventSink — 唯一事件发射出口 + persist 分流
│   ├── inbox.py                     消息队列（drain-and-merge）+ feedback waiters
│   └── tool_context.py              ToolContext Protocol — 工具收到的窄接口（ctx）
├── provider/                        模型调用层（协议隔离）
│   ├── base.py                      ModelProvider ABC + StreamAccumulator + parse_tool_args（容错，永不抛）
│   ├── openai_compat.py             OpenAI 兼容协议（httpx 流式 + 重试）
│   ├── anthropic.py                 Anthropic Messages API（thinking blocks、x-api-key）
│   ├── sse.py                       两个协议共用的 SSE 行解析
│   ├── http.py / errors.py          httpx 构造 / 带 body 的错误面（raise_with_body）
│   └── __init__.py                  create_provider() + provider registry（并发聚合模型列表）
├── store/                           SessionStore — 会话持久状态唯一所有者
│   ├── base.py                      SessionStore / MessageLog ABC + SessionMetadata
│   ├── file.py                      File 后端（history.jsonl 混合日志，零迁移）
│   └── memory.py                    Memory 后端（进程内，不落盘）
├── event/                           事件类型 + 注册表 + 序列化边界
│   ├── base.py                      WingEvent（= ChainNode）基类 + 通用系统事件
│   ├── react.py / state_change.py / query_response.py
│   └── __init__.py                  EVENT_TYPES / FACT_EVENTS 注册表 + wire_dump（WS 帧规则）
├── tools/                           内置工具
│   ├── bash.py / file.py / search.py   Bash · Read/Write/Edit · Glob/Grep
│   ├── ask_user.py / todo.py           AskUserQuestion · TodoWrite
│   ├── explorer.py                     Explorer 子 agent（只读工具集，可 run_in_background）
│   ├── experimental.py                 BetterEdit（实验，[upto] 锚点）
│   ├── shell_safety.py                 Bash 命令安全审查（白名单放行 / 默认拦截）
│   └── utils.py                        resolve_path — 相对路径按会话 workspace 解析
├── magic_command/                   prompt 命令：registry.py（元数据）+ prompt_commands.py（$ARGUMENTS 展开，无分发）
├── metrics_registry/                指标 / 审计注册中心（EventBus 订阅，原子写 JSON）
│   ├── core.py                      MetricsRegistry 类 + 单例 + 原子读写工具
│   ├── _llm_metrics.py / _tool_call_metrics.py / _compact_metrics.py
│   └── experimental.py              BetterEdit 实验审计（~/.wing/core/metrics_experimental.json）
├── common/
│   ├── logger.py                    日志初始化（按本地日期切分 + 轮转 / prune）
│   ├── tracked_list.py              TrackedList — 链拓扑引擎（I/O 委托 MessageLog）
│   ├── fs.py                        原子写（tmp + fsync + rename）/ JSON 读写
│   ├── with_retry.py                重试（指数退避 + 重试事件）
│   ├── process.py                   进程组管理（killpg 清理子进程树）
│   ├── token_counter.py             token 估算
│   └── utils.py                     session id、路径安全校验、异常链格式化
└── gateway/                         FastAPI 网关
    ├── app.py                       应用工厂（FastAPI + 路由注册）
    ├── server.py                    GatewayServer — 生命周期 + EventBus 订阅 + uptime
    ├── cli.py                       wing-gateway CLI 入口
    ├── auth.py                      opt-in API key 鉴权中间件（HTTP + WS；admin / tool_runtime）
    ├── remote_tools.py              RemoteToolManager — 远程工具宿主连接 + WS 调用分发
    ├── protocol.py                  WS + HTTP Pydantic 模型
    ├── openapi.py                   OpenAPI 元数据
    └── routes/                      session(14) · system(6) · tools(1) · health(1) · ws（事件传输 + 上行帧）
```

### 前端：`crates/wing/src/`（Rust，TUI + stdio + 编排 CLI）

```
crates/wing/src/
├── main.rs                          入口（clap；stdio 模式检测 → 过滤未知参数）
├── lib.rs                           库根：模块导出（供 bench / tests 引用；deny print_stdout/stderr）
├── cmd/                             CLI 子命令与分发
│   ├── mod.rs                       Cli/Command 定义 + dispatch（TUI / 网关生命周期 / 编排子命令 / stdio）
│   ├── args.rs                      `wing run` 与 stdio 共享的启动参数
│   ├── backend_config.rs            读 backend config（gateway host:port、wing_home）
│   ├── common.rs                    子命令共享工具（网关发现、HTTP client、输出格式化）
│   ├── discover.rs                  定位 wing-gateway 可执行文件
│   ├── start.rs / stop.rs / status.rs  网关守护进程生命周期（health + /api/shutdown）
│   ├── run.rs                       `wing run` 非阻塞启动任务（建会话 + 发 prompt，返回 session id）
│   ├── wait.rs                      `wing wait` 阻塞至会话 idle（HTTP 轮询 + WS TurnResult）
│   ├── ps.rs                        `wing ps` / `wing info`（会话列表 / 单会话运行时信息）
│   ├── messages.rs                  `wing tail` / `wing head`（消息过滤，类 Unix head/tail）
│   └── query.rs                     `wing models` / `tools` / `agents`（查询端点，表格 / JSON）
├── stdio/                           headless 前端（wing -p，Claude 协议）
│   ├── mod.rs                       run_stdio + ensure_gateway_running + 参数过滤
│   ├── ndjson.rs                    stream-json NDJSON 帧
│   ├── renderer.rs                  text / json / stream-json 输出渲染
│   └── stdin_handler.rs             SDK 双向 stdin 握手
├── gateway/client.rs                GatewayClient — WS 连接 + 读写任务
├── protocol/                        WingEvent + ClientRequest + ConnectResponse（Python 事件的 Rust 镜像）
│   ├── events.rs / client_request.rs / connect_response.rs
│   └── history.rs                   SessionMessage — 会话历史 Message 投影的 typed 镜像
├── app/                             App 状态机 + 事件循环
│   ├── mod.rs                       run_app() 主循环 + handle_event()
│   ├── runner.rs                    执行 AppIntent（HTTP/WS 副作用）
│   ├── intent.rs / transport.rs     AppIntent 枚举 + 传输抽象（WS+HTTP+client_id 原子单元，含重连退避）
│   ├── goal.rs                      Goal 编排状态机（executor/checker 循环，纯逻辑无 I/O）
│   ├── selection_panel.rs           选择面板共享内核（翻页 / 光标 / 窗口 / commit；存储归 adapter）
│   ├── ask_panel.rs                 AskUserQuestion 适配器（Tab 切题 / 多选 / 内联输入 / 确认页）
│   ├── model_panel.rs               /model 适配器（provider tab × model 行，Enter 即应用）
│   ├── replay.rs                    SyncSession 重放 → ChatCells（messages → events 能力分发）
│   ├── turn_state.rs / render_context.rs   轮次耗时 / 流式目标 cell 跟踪
│   ├── popup_state.rs               Popup + 候选缓存 + 去重
│   └── constants.rs                 协议常量（本地命令、工具名等 magic string）
├── ui/                              UI 组件
│   ├── chat_view.rs                 Chat 视图（宽度感知虚拟化）
│   ├── selection.rs                 文本选择状态机（区域标签 / 内容坐标锚定 / 区间有序化 / 快照取文本，纯逻辑）
│   ├── scrollbar.rs                 overlay 滚动条（几何 / 命中测试 / 拖拽状态机 / 绘制）
│   ├── cached_cell.rs               ChatCell 包装：渲染结果 + 高度按 generation 缓存
│   ├── panel.rs                     选择面板共享渲染（窗口数学与内核一致）
│   ├── header.rs / status_bar.rs / spinner.rs / toast.rs
│   ├── input_area/                  Composer（editing / movement / wrap / 指针映射与高亮 pointer / paste / widget / helpers）
│   ├── popup/                       command（斜杠命令 + 候选项）/ selection（通用可选列表）
│   ├── ask_select.rs                旧版必选选择器（Bash 确认）
│   └── cells/                       Chat cell 渲染（tool_call / thinking / todo_msg / ask_msg / diff_view / model_picker）
├── render/                          Markdown + 语法高亮
│   ├── markdown/                    types / parsing / code_blocks / tables / links / wrap（CJK UAX#14）
│   │   └── stream.rs                StreamingRender — 增量渲染（稳定前缀 + 活动尾部；Thinking 跳过 fence 归一化）
│   ├── syntax.rs                    syntect 高亮（two-face 主题）
│   ├── diff_highlight.rs            diff 双修订版高亮（old/new 两路状态机：删除行→old，其余→new，context 行两路都要推进）
│   └── line_utils.rs / renderable.rs
├── tui/mod.rs                       终端生命周期（init/restore、crossterm 事件流）
├── config/                          TUI 配置（mod / colors / rendering）
└── util/                            clipboard / open(链接打开) / logging / osc9（桌面通知）/ partial_json / title（OSC 0）
```

配套：`crates/wing/benches/stream_render.rs`（流式渲染基准）、`crates/wing/tests/`（stream_render 对账 / 吞吐、WS 客户端生命周期）、`crates/wing/examples/reconnect_flow_verify.rs`。

### 其他

- `crates/wing-api-client/src/` — 手写 Rust HTTP 客户端：`client.rs`（全部 API 方法）、`models.rs`、`error.rs`、`tool_host.rs`（远程工具宿主，WS 服务循环 + builder）。
- `libs/wing-sdk/wing_sdk/` — Python 远程工具宿主 SDK：`host.py`（装饰器注册 + WS 循环）、`http_client.py`、`schema.py`、`tools/`（Bash/Read/Write/Edit/Glob/Grep，workspace-bound）。
- `libs/wing-orch/wing_orch/` — 编排 CLI（后台 Goal，port of `app/goal.rs`）：`cli.py`、`goal.py`、`runner.py`。**目前少用，改动不必同步本节细节。**
- `e2e/claude-agent-sdk-integration/` — 用 claude-agent-sdk 跑 wing 的端到端测试（`make test-e2e`）。
- 测试目录：`libs/core/tests/`（后端 pytest，60 个文件）、`libs/wing-sdk/tests/`、`libs/wing-orch/tests/`。
- 顶层 `docs/dev/` 为开发者深度文档（中文），`scripts/sync_version.py` 同步版本号。

## 配置与日志

`$WING_HOME`（默认 `~/.wing`）目录布局、`config.yaml` 顶层键、前后端统一的日志策略与按日期 grep 技巧 → [docs/dev/config-logging.md](docs/dev/config-logging.md)。

## 深潜阅读（docs/dev）

AGENTS.md 保持高信息密度总览；机制级细节去 `docs/dev/`（中文）：

| 文档 | 内容 |
|------|------|
| [`docs/dev/architecture.md`](docs/dev/architecture.md) | 三层架构与数据流、TUI / stdio / 编排 CLI 三种前端形态、Goal 编排、远程工具与编排、会话生命周期与中断提交语义、事件系统与统一日志、持久化与压缩 |
| [`docs/dev/http-api.md`](docs/dev/http-api.md) | 完整 HTTP 端点表 + WebSocket 协议 + 鉴权 |
| [`docs/dev/glossary.md`](docs/dev/glossary.md) | 核心概念速查：SessionStore / MessageLog / TrackedList、工具命名空间、prompt 命令、压缩等 |
| [`docs/dev/config-logging.md`](docs/dev/config-logging.md) | WING_HOME 布局、config.yaml 键、日志轮转与查询 |

事实来源优先级：**代码 > docs/dev > AGENTS.md 概述**。若发现不一致，以代码为准并欢迎修正文档。

## 开发

```bash
# Python
uv sync
uv run wing-gateway              # 启动网关
make test-python                  # pytest
make check-python                 # ruff + ty + vulture

# Rust
cargo build
cargo test
make check-rust                   # fmt + clippy + test

# All
make test                         # Python + Rust
make check                        # Python + Rust
make fmt                          # 格式化全部
```

## 分发

- **Python**：`pip install wing-agent` → 安装 `wing-gateway`（Python 网关）与 `wing-cli`（maturin 构建的 Rust 二进制，提供 `wing` 命令）。
- **Rust**：GitHub Release 预编译二进制 → `wing` CLI（TUI + stdio + 守护进程控制）。
- **SDK/Orch**：`wing-sdk` / `wing-orch` 为 uv workspace 包（`libs/`），未发布 PyPI。

## 提交信息

```
type(scope): short description

[optional body]
```

Types：`feat`, `fix`, `refactor`, `test`, `docs`, `chore`。

Scopes 沿用模块边界：`gateway`, `runtime`, `session`, `tui`, `protocol`, `tools`, `config` 等。

示例：

```
feat(gateway): add HTTP session/fork endpoint
refactor(runtime): clean up WingRuntime as service layer
fix(protocol): remove session_id from ConnectResponse
test(gateway): add HTTP endpoint unit tests
```

首行不超过 72 字符；body 讲 *why*，不讲 *what*。
