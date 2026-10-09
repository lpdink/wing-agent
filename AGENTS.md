# wing-agent

Monorepo：Python agent runtime（`libs/core/wing/`，pip 包 `wing-gateway`）+ Rust 前端（`crates/wing/`，一个 `wing` 二进制，提供 TUI / stdio / ACP / 编排 CLI 四种形态）。

> **维护约定**：本文件只保留高信息密度总览。**不要随手往里加东西，除非 user 明确要求或同意**；机制细节、临时知识、踩坑记录一律写进 `docs/dev/`（见文末「深潜阅读」登记表）。

## 架构

```
┌─────────────────────────────┐             ┌─────────────────────────┐               ┌─────────────────────────┐
│ Frontends（wing 二进制）    │             │ Gateway (FastAPI)       │               │ Runtime (Python)        │
│ TUI（默认）· ratatui 循环   │──── WS ────►│ GatewayServer           │──── HTTP ────►│ WingRuntime（协调者）   │
│ stdio（wing -p）· NDJSON    │             │ · routes/session(16)    │               │ ├ SessionManager        │
│ acp（wing acp）· ACP v1     │             │ · routes/system(6)      │               │ ├ SessionStore          │
│ 编排 CLI · run/wait/ps/…    │◄── 事件 ────│ · routes/tools · health │◄──────────────│ ├ ContextManager        │
│ 网关生命周期 · start/stop   │             │ · routes/ws（事件流）   │               │ ├ EventBus              │
│ GatewayClient(WS)+ApiClient │             │ auth（opt-in）          │               │ └ provider/（LLM 调用） │
│ HTTP 建会话 → WS 订阅       │             │                         │               │                         │
└─────────────────────────────┘             └─────────────────────────┘               └─────────────────────────┘
```

- **四种前端形态，同一个二进制**：TUI（默认，human-in-the-loop）；stdio（`wing -p`，headless，Claude Code 兼容 NDJSON——把 `wing` alias 为 `claude` 即可接入外部编排器）；ACP（`wing acp`，stdio 上的 Agent Client Protocol v1 agent 服务端，桥接本机网关供 Zed / omnigent 等驱动）；编排 CLI（`wing run/wait/ps/info/tail/head/release` 后台任务，`wing start/stop/status` 网关生命周期）。
- **协议**：HTTP 承载生命周期 / 查询 / 变更（24 个 RPC 端点）；WebSocket（`/ws`）只承载实时 ReAct 事件流 + 客户端上行帧（message / Ask 回答 / tool_call_result）。会话创建与 WS 握手解耦：先 HTTP 建会话，再订阅事件。API key 鉴权在网关 opt-in（HTTP header / WS query param），TLS 交给反向代理。
- **持久化**：`SessionStore` 是会话全部持久状态（metadata、混合 message/event 日志、aux）的唯一所有者；后端 `file`（默认，`~/.wing/core/sessions/`）与 `memory`（进程内）。`TrackedList` 是纯内存链拓扑引擎（uuid/parentUuid），I/O 全部委托 `MessageLog`；SQL 后端是增量实现，非架构改动。
- **模型调用**：`provider/` 隔离协议差异（OpenAI 兼容 / Anthropic），ReAct 循环对协议无感知；provider 实例无状态、归全进程共享池（`provider/pool.py`），会话级参数（session id 缓存亲和 / media / thinking）经 `RequestOptions` 每次调用注入。

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
├── __init__.py / _version.py        包入口（零 import 副作用）/ 版本号
├── build_info.py                    构建信息读取口：版本 + commit hash（构建时注入，运行期零 git）
├── runtime.py                       WingRuntime — service 层协调者（post() 唯一入站，路由到 Session/CM）
├── system.py                        系统级热重载流程（config → hooks → commands → provider → skills/rules；runtime 委托）
├── session/                         会话生命周期包（公共 API 经 __init__ re-export）
│   ├── session.py                   Session — messages + state + metadata（经 SessionStore）
│   ├── manager.py                   SessionManager — 多会话、fork/resume、store registry
│   ├── reaper.py                    SessionReaper — 空闲会话逐出（触摸订阅 + 扫描）
│   ├── template.py                  AgentTemplate — 配置 agents: 的 model/tools/prompt/skills/rules
│   └── override.py                  AgentOverride — 创建期参数覆盖（领域类型，住领域层非网关）
├── chain.py                         TrackedList — 链拓扑引擎（I/O 委托 MessageLog）
├── context/                         上下文域包：窗口投影 / 声明集 / 压缩 / 资源加载
│   ├── manager.py                   ContextManager — 窗口投影 / 声明集 / rewind / pending compact 编排
│   ├── compaction.py                Compactor — 压缩策略（LLM 摘要）+ PendingCompact / LLMMessagesResult
│   └── resources.py                 skills/rules 文件加载（glob + frontmatter，与消息链无关）
├── background.py                    BackgroundScheduler — 周期任务宿主（逐出 / 未来 dreaming 等）
├── config/                          配置包：Config 模型 + WING_HOME 解析（公共 API 经 __init__ re-export）
│   ├── models.py                    配置模型 + resolve_model_capabilities / resolve_model_display_name
│   ├── spec.py                      声明层：S(...) / SettingMeta / ApplyScope（字段元信息唯一来源）
│   ├── problems.py                  跨字段检查纯函数 + ConfigProblem（加载期 / 设置面板共用）
│   ├── catalog.py                   设置目录树：SettingNode / build_catalog() / parse_path()
│   ├── emit.py                      规范形 YAML emitter（默认模板与保存共用；注释来自声明）
│   ├── loader.py                    get_wing_home / get_config_path / load_config / get_config / reset_config
│   └── user_agent.py                UA 预设（opencode / qwen-code）+ get_headers
├── schema/                          领域模型包：Tool / ToolParam / Message 等核心 schema（公共 API 经 __init__ re-export）
│   ├── message.py                   ChainNode / 内容块 / MediaRef / Message（落盘格式守门人）
│   ├── llm.py                       LLMUsage / LLMResponse / ToolCall / ToolCallDelta / PendingCall
│   └── tool.py                      ToolError / ToolParam / Tool / ToolOutput / AgentSkill
├── media/                           图片媒体纯函数层包（id/格式/尺寸/信封/请求期投影）
│   ├── facts.py                     字节 → 事实（sha256 id / mime / 尺寸 / 信封 / 估算）
│   └── policy.py                    请求期投影（高水位 + 量子驱逐）+ 占位常量
├── tool_registry.py                 ToolRegistry — 命名空间感知注册表 + ToolRef 解析
├── event_bus.py                     EventBus — 全局单例事件路由
├── hooks/                          Hook 扩展点：注册表 + 配置文件加载（公共 API 经 __init__ re-export）
│   ├── registry.py                  HookRegistry 管道 + 全局单例 hooks（before_session_start / before_user_message / before_tool_call / after_tool_call）
│   └── loader.py                    load_hooks — glob 匹配 .py 并 import（注册经 hooks.on() 装饰器）
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
│   ├── transport.py                 HTTP/SSE 传输管道与错误面（SSE 行解析 / 空闲超时 / httpx 构造 / raise_with_body）
│   ├── media.py                     请求期媒体投影与序列化原语（两协议共用）
│   ├── factory.py                   create_provider() — 按协议创建 provider 实例
│   ├── pool.py                      共享 provider 池（每 name 一个无状态实例；全会话与 /api/models 聚合共用；reload 换新 + 旧实例退场）
│   ├── openai/                      OpenAI 兼容协议子包（provider / serialize / stream）
│   ├── anthropic/                   Anthropic 协议子包（provider / serialize / stream）
│   └── __init__.py                  ModelProvider + create_provider（稳定入口）
├── store/                           SessionStore — 会话持久状态唯一所有者
│   ├── base.py                      SessionStore / MessageLog ABC + SessionMetadata
│   ├── file.py                      File 后端（history.jsonl 混合日志，零迁移）
│   └── memory.py                    Memory 后端（进程内，不落盘）
├── event/                           事件类型 + 注册表 + 序列化边界
│   ├── base.py                      WingEvent（= ChainNode）基类 + 通用系统事件
│   ├── react.py / state_change.py / query_response.py
│   └── __init__.py                  EVENT_TYPES / FACT_EVENTS 注册表 + wire_dump（WS 帧规则）
├── tools/                           内置工具（__init__ 显式导入 = 注册）
│   ├── builtin/                     一工具一文件：bash · read/write/edit · glob/grep · read_image · ask_user · todo
│   └── internal/                    工具基础设施：utils（resolve_path）· rg（_run_rg）· diff_window · shell_safety
├── commands.py                      prompt 命令：元数据注册表 + $ARGUMENTS 展开（无分发）
├── audit/                           指标 / 审计注册中心（EventBus 订阅，原子写 JSON；install() 由组合根显式调用）
│   ├── core.py                      MetricsRegistry 类 + 单例 + 原子读写工具
│   └── _llm_metrics.py / _tool_call_metrics.py / _compact_metrics.py
├── common/
│   ├── logger.py                    日志初始化（按本地日期切分 + 轮转 / prune）
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
    ├── frames.py                    出网帧切分（>8 MiB 载荷按 UTF-8 边界切为 ≤16 MiB 帧）
    ├── projection.py                领域 → 协议响应投影（session info / branches）
    ├── protocol/                    协议模型包（消费方从包根 import）：ws · session · system · errors
    ├── openapi.py                   OpenAPI 元数据
    └── routes/                      session(15) · system(6) · tools(1) · health(1) · ws（事件传输 + 上行帧）
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
│   ├── release.rs                   `wing release` 逐出会话内存态（显式 eviction，幂等）
│   ├── messages.rs                  `wing tail` / `wing head`（消息过滤，类 Unix head/tail）
│   └── query.rs                     `wing models` / `tools` / `agents`（查询端点，表格 / JSON）
├── acp/                             ACP 前端（wing acp，stdio 上的 Agent Client Protocol 服务端）
│   ├── mod.rs                       入口（ensure gateway → WS 连接 → HTTP client → 服务循环）+ CLI 参数
│   ├── agent.rs                     ACP handler 注册（initialize / new / prompt / cancel / list / load / resume / close / set_config_option）
│   ├── model.rs                     模型 config option（值域 / 分组 / currentValue）+ 热切换 + 外部变更中继
│   ├── session.rs                   SessionHub：会话表 · WS 事件泵与分流 · 挂载（load/resume）与回收（close）· prompt 串行化
│   └── translate.rs                 WingEvent → ACP session/update 映射（工具卡片状态 + prompt 拍平 + 终态判定）
│       └── replay.rs                session/load 的历史回放投影（sync_session → update 序列）
├── stdio/                           headless 前端（wing -p，Claude 协议）
│   ├── mod.rs                       run_stdio + ensure_gateway_running + 参数过滤
│   ├── ndjson.rs                    stream-json NDJSON 帧
│   ├── renderer.rs                  text / json / stream-json 输出渲染
│   ├── stdout.rs                    stdout 串行化写口（renderer 与 stdin pump 共享）
│   └── stdin_handler.rs             常驻 stdin pump（initialize / interrupt 应答 + 收尾）
├── gateway/client.rs                GatewayClient — WS 连接 + 读写任务
├── protocol/                        WingEvent + ClientRequest + ConnectResponse（Python 事件的 Rust 镜像）
│   ├── events.rs / client_request.rs / connect_response.rs
│   └── history.rs                   SessionMessage — 会话历史 Message 投影的 typed 镜像
├── shared/                          中立层：App 与 UI 共享的状态机与词汇（不依赖 app / ui）
│   ├── panels/mod.rs                选择面板内核（翻页 / 光标 / 窗口 / commit；存储归 adapter）
│   ├── panels/ask.rs                ask 模型与归一化入口（AskUserQuestion 面板 / Bash 确认的必选形态 / 只读提示）
│   ├── panels/picker.rs             /model 适配器（provider tab × model 行，Enter 即应用）
│   ├── pinning.rs                   会话置顶（pin）约定：`pin` 标签 + 「置顶在前、后 pin 更靠前」的唯一排序实现（后端零感知）
│   ├── tips.rs                      开屏提示池（欢迎屏轮换一条 + /tips 面板全量）
│   └── constants.rs                 协议常量（本地命令、工具名等 magic string）
├── app/                             App 状态机 + 事件循环
│   ├── mod.rs                       run_app() 主循环 + handle_event()
│   ├── runner.rs                    执行 AppIntent（HTTP/WS 副作用）
│   ├── intent.rs / transport.rs     AppIntent 枚举 + 传输抽象（WS+HTTP+client_id 原子单元，含重连退避）
│   ├── images.rs                   图片 lane：能力/配置门 · ImageStore 持有 · 元数据表 · 帧末绘制（遮挡与选择门）· 新鲜度检查（1s 节流 · 可注入时钟）
│   ├── replay.rs                    SyncSession 重放 → ChatCells（messages → events 能力分发）
│   ├── turn_state.rs / render_context.rs   轮次耗时 / 流式目标 cell 跟踪
│   └── popup_state.rs               Popup + 候选缓存 + 去重
├── ui/                              UI 组件
│   ├── chat_view/                   Chat 视图：mod（ChatView 结构）· cell（ChatCell 渲染）· model（内容模型）· viewport（滚动·几何·高度缓存·绘制）· frame（帧快照·选择映射）· link（链接表·OSC8）· image（图片放置表与候选路径）
│   ├── selection.rs                 文本选择状态机（区域标签 / 内容坐标锚定 / 区间有序化 / 快照取文本，纯逻辑）
│   ├── scrollbar.rs                 overlay 滚动条（几何 / 命中测试 / 拖拽状态机 / 绘制）
│   ├── cached_cell.rs               ChatCell 包装：渲染结果 + 高度按 generation 缓存 + CellFrame 投影（链接 / 图片锚点侧信道）
│   ├── image/                       终端图形（唯一 door to ratatui-image/image）：probe（能力探测·可注入）· store（worker+LRU+epoch + 上限：文件/像素/缓存张数与字节/memo）· place（paint 原语）
│   ├── panel.rs                     选择面板共享渲染（窗口数学取自 shared/panels 内核）
│   ├── shimmer.rs                   扫光 / 混色原语（开屏 wordmark 与思考块标题行共用）
│   ├── welcome/                     开屏欢迎屏：mod（状态·宽度阶梯·可见性门控）· art（海鸥帧 + 像素大字数据）· sprite（半格渲染 + 品牌调色板）· motion（idle/干活动作规划）· wordmark（渐变 + 扫光）
│   ├── status_bar.rs / spinner.rs / toast.rs
│   ├── input_area/                  Composer 悬浮卡片（chrome：悬浮几何·活动栏·元信息栏 / model：段 + 粘贴 chip 注册表 / widget / editing / movement / wrap / 指针映射与高亮 pointer / paste / helpers）
│   ├── popup/                       command（斜杠命令 + 候选项）/ selection（通用可选列表）
│   └── cells/                       Chat cell 渲染（tool_call / thinking / todo_msg / ask_msg / diff_view / model_picker / notice）
├── render/                          Markdown + 语法高亮
│   ├── markdown/                    types / parsing / code_blocks / tables / links / wrap（CJK UAX#14）/ images（图片锚点）/ math（公式渲染）
│   │   └── stream.rs                StreamingRender — 增量渲染（稳定前缀 + 活动尾部；Thinking 跳过 fence 归一化）
│   ├── syntax.rs                    syntect 高亮（two-face 主题）
│   ├── diff_highlight.rs            diff 双修订版高亮（old/new 两路状态机：删除行→old，其余→new，context 行两路都要推进）
│   ├── fit.rs                       像素↔字符格共享装填（fit_cells；布局与编码同一份数学）
│   └── line_utils.rs / renderable.rs
├── tui/mod.rs                       终端生命周期（init/restore、crossterm 事件流）
├── config/                          TUI 配置（mod / colors / rendering）
└── util/                            clipboard / open(链接打开) / logging / osc9（桌面通知）/ partial_json / title（OSC 0）
```

配套：`crates/wing/benches/stream_render.rs`（流式渲染基准）、`crates/wing/benches/image_frame.rs`（图片：每帧/滚动/首次编码/新鲜度检查）、`crates/wing/tests/`（stream_render 对账 / 吞吐、WS 客户端生命周期、layer_guard 分层守门）、`crates/wing/examples/`（reconnect_flow_verify；welcome_preview 开屏预览）。

### 其他

- `crates/wing-api-client/src/` — 手写 Rust HTTP 客户端：`client.rs`（全部 API 方法）、`models.rs`、`error.rs`、`tool_host.rs`（远程工具宿主，WS 服务循环 + builder）。
- `crates/wing-math/` — LaTeX 数学子集 → 终端字符网格（借鉴内联 `term-maths` + `rust-latex-parser`，附出处/许可）：窄接口 `render_inline` / `render_display` / `render_block`，`None` = 「不该由引擎渲染，请显示源码」。接线（事件、定界符归一化、降级）在 `crates/wing/src/render/markdown/math.rs` → [docs/dev/tui-rendering.md](docs/dev/tui-rendering.md) 第二节·五。
- `libs/wing-sdk/wing_sdk/` — Python 远程工具宿主 SDK：`host.py`（装饰器注册 + WS 循环）、`http_client.py`、`schema.py`、`tools/`（Bash/Read/Write/Edit/Glob/Grep，workspace-bound）。
- `assets/` — 品牌与演示素材（README 页头 banner 明暗两版、站姿 mascot SVG、社交预览 PNG、README 的 demo/速度 GIF）：SVG 由 `examples/export_logo.rs` 从欢迎屏的同一份像素网格导出，README 的 GIF 由 `scripts/demo/`（假 Provider 喂真 TUI，`make demo`）录制后挂在 `readme-assets` rolling release 上（不进 git），性能数字由 `scripts/demo/latency.py` 现量 —— 不会漂移 → [scripts/demo/README.md](scripts/demo/README.md)。
- `libs/wing-probe/` — 确定性集成测试基础设施（假 Provider + driver + observer 断言库）：`wing_probe/`（env / provider / driver / watch / history / files / toolhost）、`scenarios/`（整机断言场景）、`tests/`（基础设施自测）。**禁止 import `wing`**（AST 门禁强制；允许 `wing_sdk`），一切经公开 HTTP / WS 协议 → [docs/dev/probe-testing.md](docs/dev/probe-testing.md)。
- `extensions/vscode/` — VSCode 前端（编辑器内的接入面，与 `wing` 二进制的四种形态并列；TS strict + pnpm 单包四层：`src/core` 网关能力层 / `src/host` 扩展宿主 / `src/webview` React 渲染 / `src/shared` 两侧契约）。层门禁由机制强制：分 tsconfig（DOM/node 隔离）+ ESLint 分区规则 + `tests/layers` 守门测试；`make check`/`make test` 含 `check-ts`/`test-ts`，CI 有 `typescript-check` job → [docs/dev/vscode-extension.md](docs/dev/vscode-extension.md) · [extensions/vscode/README.md](extensions/vscode/README.md)。
- 测试目录：`libs/core/tests/`（后端 pytest，81 个测试文件 + `conftest.py`）、`libs/wing-sdk/tests/`。
- 顶层 `docs/dev/` 为开发者深度文档（中文），`scripts/sync_version.py` 同步版本号。

## 配置与日志

`$WING_HOME`（默认 `~/.wing`）目录布局、`config.yaml` 顶层键、前后端统一的日志策略与按日期 grep 技巧 → [docs/dev/config-logging.md](docs/dev/config-logging.md)。

## 深潜阅读（docs/dev）

AGENTS.md 保持高信息密度总览；机制级细节去 `docs/dev/`（中文）：

| 文档 | 内容 |
|------|------|
| [`docs/dev/architecture.md`](docs/dev/architecture.md) | 三层架构与数据流、TUI / stdio / ACP / 编排 CLI 四种前端形态、远程工具、会话生命周期与中断提交语义、事件系统与统一日志、持久化与压缩 |
| [`docs/dev/backend-layout.md`](docs/dev/backend-layout.md) | 后端分层规范（`libs/core/wing/**`）：分层图与依赖方向、每包职责一句话、迁移映射（历史记录）、分层守门测试（`test_layering.py`） |
| [`docs/dev/http-api.md`](docs/dev/http-api.md) | 完整 HTTP 端点表 + WebSocket 协议 + 鉴权 |
| [`docs/dev/glossary.md`](docs/dev/glossary.md) | 核心概念速查：SessionStore / MessageLog / TrackedList、工具命名空间、prompt 命令、压缩等 |
| [`docs/dev/config-logging.md`](docs/dev/config-logging.md) | WING_HOME 布局、config.yaml 键、日志轮转与查询 |
| [`docs/dev/media-images.md`](docs/dev/media-images.md) | 媒体与图片（read-image）：ReadImage 工具、内容寻址媒体池、模型能力声明、请求期图片投影（高水位 + 量子批量驱逐）与 KV/前缀 cache、inline/followup 线格式、probe 场景清单 |
| [`docs/dev/tui-rendering.md`](docs/dev/tui-rendering.md) | TUI markdown 渲染：`render_probe` 调试入口、Content/Thinking 两个 profile 的差异、公式（`$…$` / `$$…$$` / AMS 环境）渲染与定界符归一化、流式静息态 == 参考渲染的不变量、已知边界、图片锚点的行数纯函数与路径策略 |
| [`docs/dev/tui-images.md`](docs/dev/tui-images.md) | TUI 图片能力：两档阶梯（可渲染 / 存量链接）、探测与配置、三态、资源上限与压力验证、**新鲜度**（重写同一路径 ≤1s 换图）、失效触发点、遮挡与选择、性能数字、真机验收清单、症状→先看哪里 |
| [`docs/dev/vscode-extension.md`](docs/dev/vscode-extension.md) | VSCode 扩展（`extensions/vscode/`）：四层分层与数据流、桥协议与归约（重放==直播 / 单 WS 多订阅）、会话时序与多 Tab、连接自愈、构建门禁 / smoke / 打包与验收 |
| [`docs/dev/probe-testing.md`](docs/dev/probe-testing.md) | 确定性集成测试（wing-probe）：跑法 / 新增断言场景（写代码、不写配置）/ 断言原语速查 / 上下文红线清单与 persist 口径 / 逃生舱约定 |
| [`docs/dev/welcome-mascot.md`](docs/dev/welcome-mascot.md) | 开屏海鸥：字母网格帧数据与品牌调色板、待机/干活两姿态与动作族、可见性门控的重绘成本契约、改画与预览的创作期工作流 |

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
make check-rust                   # cargo fmt --check + clippy

# All
make test                         # 只跑测试：Python + Rust + TS + probe
make test-probe                   # 确定性集成场景（wing-probe，离线、无外部 API key）
make check                        # 只跑静态检查：Python + Rust + TS（含 extensions/vscode 门禁）
make fmt                          # 格式化全部
```

**后端特性测试约定**：开发后端特性（新增 / 修改 `libs/core` 的行为——上下文链、事件、协议、工具、网关等）时，**必须在 `libs/wing-probe/` 下增加真实有效的测试**：断言场景用代码写（`scenarios/`，不写配置文件），经公开 HTTP / WS 协议驱动"真网关 + 假 Provider"；`make test-probe` 与 CI 的 `probe-check` job 会强制其通过。**上下文红线**（compact / rewind / fork 等一切对上下文的操作）的行为变更必须配套红线断言。详见 [docs/dev/probe-testing.md](docs/dev/probe-testing.md)。

## 分发

- **Python**：`pip install wing-agent` → 安装 `wing-gateway`（Python 网关）与 `wing-cli`（maturin 构建的 Rust 二进制，提供 `wing` 命令）。
- **Rust**：GitHub Release 预编译二进制 → `wing` CLI（TUI + stdio + 守护进程控制）。
- **SDK**：`wing-sdk` 为 uv workspace 包（`libs/`），未发布 PyPI。

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

**语言与内容**（commit message 与 PR 一视同仁）：

- commit message 与 PR 标题一律英文（PR 正文中文）；
- 只描述改动本身（改了什么、为什么），不写产出过程信息：是否经过审查、分级结论（如 B/S/N）、返修轮次、内部编排代号等，一律不写；
- 已有强制校验：commit message 走 `commit-msg` 钩子（含 CJK 即拒），PR 标题走 `pr-title` workflow；新克隆需先 `git config core.hooksPath .githooks` 启用钩子。
