# wing-agent

Monorepo：Python agent runtime（`libs/core/wing/`，pip 包 `wing-gateway`）+ Rust 前端（`crates/wing/`，一个 `wing` 二进制，提供 TUI / stdio / ACP / 编排 CLI 四种形态）。

> **维护约定**：本文件只保留高信息密度总览。**不要随手往里加东西，除非 user 明确要求或同意**；机制细节、临时知识、踩坑记录一律写进 `docs/dev/`（见文末「深潜阅读」登记表）。

## 架构

```
┌─────────────────────────────┐             ┌─────────────────────────┐               ┌─────────────────────────┐
│ Frontends（wing 二进制）    │             │ Gateway (FastAPI)       │               │ Runtime (Python)        │
│ TUI（默认）· ratatui 循环   │──── WS ────►│ GatewayServer           │──── HTTP ────►│ WingRuntime（协调者）   │
│ stdio（wing -p）· NDJSON    │             │ · routes/session(22)    │               │ ├ SessionManager        │
│ acp（wing acp）· ACP v1     │             │ · routes/system(6)      │               │ ├ SessionStore          │
│ 编排 CLI · run/wait/ps/…    │◄── 事件 ────│ · routes/settings(4)    │◄──────────────│ ├ ContextManager        │
│ 网关生命周期 · start/stop   │             │ · wake(3) · tools/…     │               │ ├ EventBus              │
│ GatewayClient(WS)+ApiClient │             │ auth（opt-in）          │               │ └ provider/（LLM 调用） │
│ HTTP 建会话 → WS 订阅       │             │                         │               │                         │
└─────────────────────────────┘             └─────────────────────────┘               └─────────────────────────┘
```

- **四种前端形态，同一个二进制**：TUI（默认，human-in-the-loop）；stdio（`wing -p`，headless，Claude Code 兼容 NDJSON——把 `wing` alias 为 `claude` 即可接入外部编排器）；ACP（`wing acp`，stdio 上的 Agent Client Protocol v1 agent 服务端，桥接本机网关供 Zed / omnigent 等驱动）；编排 CLI（`wing run/wait/ps/info/usage/tail/head/release` 后台任务，`wing watch/wake` 会话唤醒，`wing start/stop/status` 网关生命周期）。
- **协议**：HTTP 承载生命周期 / 查询 / 变更（42 个 RPC 端点）；WebSocket（`/ws`）只承载实时 ReAct 事件流 + 客户端上行帧（message / Ask 回答 / tool_call_result）。会话创建与 WS 握手解耦：先 HTTP 建会话，再订阅事件。API key 鉴权在网关 opt-in（HTTP header/query · WS header/query），TLS 交给反向代理。CORS 默认放通 loopback 来源（外部来源走 `gateway.cors.origins`；推荐同源部署）。
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
│   ├── fork.py                      fork 的数据搬迁：记录切片 / 拓扑重映射 / 媒体复制（纯函数，无 self）
│   ├── reaper.py                    SessionReaper — 空闲会话逐出（触摸订阅 + 扫描）
│   ├── archiver.py                  SessionArchiver — 过期会话自动归档（读配置 + 一次扫描）
│   ├── template.py                  AgentTemplate — 配置 agents: 的 model/tools/prompt/skills/rules
│   ├── model_binding.py             生效模型绑定（resume 链的只读投影；会话列表的模型列）
│   ├── usage.py                     UsageTracker — 会话用量账本（事件驱动累计 + 节流落盘）
│   ├── tags.py                      会话标签（会话元数据里的 opaque 字符串 + `pin` 标签约定）
│   └── override.py                  AgentOverride — 创建期参数覆盖（领域类型，住领域层非网关）
├── chain.py                         TrackedList — 链拓扑引擎（I/O 委托 MessageLog）
├── context/                         上下文域包：窗口投影 / 声明集 / 压缩 / 资源加载 / 缓存账本
│   ├── manager.py                   ContextManager — 窗口投影 / 声明集 / rewind / pending compact 编排 / 缓存分段
│   ├── compaction.py                Compactor — 压缩策略（LLM 摘要）+ PendingCompact / LLMMessagesResult
│   ├── resources.py                 skills/rules 文件加载（glob + frontmatter，与消息链无关；加载期采 src 事实）
│   ├── cache_ledger.py              缓存账本 — 分段指纹 / 理论命中率 / 分叉归因 / 端点投影（best-effort 记账）
│   └── cache_store.py               账本的唯一 SQL 处：cache_call / cache_segment（进程级 wing.db）
├── metrics/                         用量统计包（L3）：审计流水 + 会话目录的聚合（`store` 读、`catalog` 写）
│   ├── store.py                     唯一 SQL 处：audit_* / session_catalog 的聚合（窗口 + 各维度）
│   ├── measures.py                  度量词表（11 个整数）+ 行 / 窗口值类型（纯函数）
│   ├── report.py                    查询编排：窗口 → 各维度的行 → wire 投影
│   └── catalog.py                   session_catalog 的写入与启动回填（派生投影）
├── background.py                    BackgroundScheduler — 周期任务宿主（逐出 / 自动归档 / 会话目录回填）
├── config/                          配置包：Config 模型 + WING_HOME 解析（公共 API 经 __init__ re-export）
│   ├── models.py                    配置模型 + resolve_model_capabilities / resolve_model_display_name
│   ├── spec.py                      声明层：S(...) / SettingMeta / ApplyScope（字段元信息唯一来源）
│   ├── problems.py                  跨字段检查纯函数 + ConfigProblem（加载期 / 设置面板共用）
│   ├── catalog.py                   设置目录树：SettingNode / build_catalog() / parse_path()
│   ├── groups.py                    业务分组表：SETTING_GROUPS / build_groups()（界面分类的唯一声明处）
│   ├── emit.py                      规范形 YAML emitter（默认模板与保存共用；只有分隔行与注释掉的默认值）
│   ├── document.py                  稀疏文档视图（读 / 指纹 / 密文三态 / 未知键 / changed_paths）
│   ├── boot.py                      启动读取（永不抛：模板生成 / 语法错 / 校验不过 → 降级启动）
│   ├── loader.py                    get_wing_home / get_config_path / load_config / get_config / reset_config
│   └── user_agent.py                UA 预设（opencode / qwen-code）+ get_headers
├── schema/                          领域模型包：Tool / ToolParam / Message 等核心 schema（公共 API 经 __init__ re-export）
│   ├── message.py                   ChainNode / 内容块 / MediaRef / Message（落盘格式守门人）
│   ├── llm.py                       LLMUsage / LLMResponse / ToolCall / ToolCallDelta / PendingCall
│   ├── tool.py                      ToolError / ToolParam / Tool / ToolOutput / AgentSkill
│   └── cache.py                     缓存账本的段 label / 规范形 hash / CachePart / CacheCall / CacheProbe
├── media/                           图片媒体纯函数层包（id/格式/尺寸/信封/上传闸门与拒绝文案/请求期投影）
│   ├── facts.py                     字节 → 事实（sha256 id / mime / 尺寸 / 信封 / 估算 / 上传闸门）
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
│   ├── diagnostics.py               worker 协程栈内省（`_report_undead_worker` 的取证工具，纯 stdlib）
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
├── store/                           SessionStore — 会话持久状态唯一所有者（含归档区：archive / unarchive / list_archived）
│   ├── base.py                      SessionStore / MessageLog 两个 ABC（只留抽象，无实现）
│   ├── models.py                    落盘模型（FileRef / SessionUsage / SessionMetadata / TagMeta / SessionSummary / ArchivedSession）
│   ├── names.py                     路径组件闸门（媒体 id / 文件名 / 内容寻址校验）
│   ├── records.py                   raw record 规范形（canonical_record）+ 标题回退
│   ├── file.py                      File 后端（history.jsonl 混合日志，零迁移）
│   └── memory.py                    Memory 后端（进程内，不落盘）
├── event/                           事件类型 + 注册表 + 序列化边界
│   ├── base.py                      WingEvent（= ChainNode）基类 + 通用系统事件
│   ├── react.py / state_change.py / query_response.py
│   └── __init__.py                  EVENT_TYPES / FACT_EVENTS 注册表 + wire_dump（WS 帧规则）
├── tools/                           内置工具（__init__ 显式导入 = 注册）
│   ├── builtin/                     一工具一文件：bash · read/write/edit · glob/grep · read_image · ask_user · todo
│   └── internal/                    工具基础设施：utils（resolve_path）· rg（_run_rg）· diff_window · shell_safety · io_errors（文件类工具的错误包装）· feedback（等人回答的超时） · write_target（就地写 vs 原子替换的唯一判定）
├── commands.py                      prompt 命令：元数据注册表 + $ARGUMENTS 展开（无分发）
├── db/                              进程级 SQLite 门面（$WING_HOME/core/wing.db：迁移 + 降级 + 跨线程事务；与 store/ 的会话持久化正交）
│   ├── database.py                  Database（WAL / user_version 迁移 / 跨线程单连接 + 事务 / 打开失败即降级）
│   └── migrations.py                MIGRATIONS 可追加 tuple（v1 = 审计三张表，v2 = wake_trigger，v3 = 缓存账本，v4 = audit_user_message + session_catalog）+ SCHEMA_VERSION
├── audit/                           审计：事件 → wing.db 的 audit_* 追加行（LLM 调用 / 工具调用 / 压缩 / 用户消息；EventBus 订阅，install() 由组合根显式调用）
│   └── recorder.py                  AuditRecorder — LLM 调用 / 工具调用 / 压缩 / 用户消息各一条 INSERT
├── wake/                            会话唤醒（wake_trigger 表 + 事件/时间驱动 + 内置信封；投递经注入的 WakePorts）
│   ├── spec.py                      spec 语法与时间推进（in/at/every/daily；纯函数）
│   ├── model.py                     WakeTrigger + 状态/事件常量 + on_events 解析
│   ├── store.py                     WakeStore — wake_trigger 的唯一 SQL 处
│   ├── envelope.py                  内置信封渲染（done / ask / timer；纯函数）
│   ├── scheduler.py                 WakeScheduler — 单 asyncio task + 可注入时钟 + notify()
│   ├── ports.py                     WakePorts Protocol（session_exists / is_archived / deliver）
│   └── service.py                   WakeService — 校验 / 生命周期 / 事件分发 / 投递 / 重启恢复
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
    ├── setup_guard.py               setup mode 守门（allowlist 白名单 + 503 error="setup_mode"）+ SetupModeError
    ├── remote_tools.py              RemoteToolManager — 远程工具宿主连接 + WS 调用分发
    ├── cors.py                      跨源访问装配（默认放通 loopback 来源；外部来源走 gateway.cors.origins）
    ├── frames.py                    出网帧切分（>8 MiB 载荷按 UTF-8 边界切为 ≤16 MiB 帧）
    ├── projection.py                领域 → 协议响应投影（session info / branches）
    ├── protocol/                    协议模型包（消费方从包根 import）：ws · session · system · settings · wake · usage · errors
    ├── openapi.py                   OpenAPI 元数据
    ├── workspace/                   只读工作区文件浏览（自含子包，拆 Gateway 可带走）：service · protocol · routes（4 个端点）
    └── routes/                      session(22) · system(6) · settings(4) · wake(3) · usage(1) · tools(1) · health(1) · ws（事件传输 + 上行帧）· _common（取 server / 客户端 id / 会话存在性 / 上传预检的机械样板）
```

### 前端：`crates/wing/src/`（Rust，TUI + stdio + ACP + 编排 CLI）

```
crates/wing/src/
├── main.rs                          入口（args_os 收集 + UTF-8 校验 → clap；stdio 模式检测 → 过滤未知参数）
├── lib.rs                           库根：模块导出（供 bench / tests 引用；deny print_stdout/stderr）
├── cmd/                             CLI 子命令与分发
│   ├── mod.rs                       Cli/Command 定义 + dispatch（TUI / 网关生命周期 / 编排子命令 / stdio）
│   ├── args.rs                      `wing run` 与 stdio 共享的启动参数
│   ├── argv.rs                      命令行参数入口（OsString → UTF-8 String；非法字节友好报错、exit 2）
│   ├── backend_config.rs            读 backend config（gateway host:port、wing_home）
│   ├── common.rs                    子命令共享工具（网关发现、HTTP client、输出格式化、404→resume 水合惯例）
│   ├── discover.rs                  定位 wing-gateway 可执行文件
│   ├── exit_summary.rs              TUI 退出摘要（终端恢复后打印；网关不可达时降级）
│   ├── start.rs / stop.rs / status.rs / restart.rs  网关守护进程生命周期（health + /api/shutdown）
│   ├── run.rs                       `wing run` 非阻塞启动任务（建会话 + 发 prompt，返回 session id）
│   ├── wait.rs                      `wing wait` 阻塞至会话 idle（HTTP 轮询 + WS TurnResult）
│   ├── ps.rs                        `wing ps` / `wing info`（会话列表 / 单会话运行时信息）
│   ├── cache.rs                     `wing cache`（KV 前缀缓存账本：理论/实际命中率 + 分叉归因）
│   ├── usage.rs                     `wing usage`（我的使用史：活动热力图 + 7 个 `--by` 维度 + `--json` / `-w`）
│   ├── branch.rs                    `wing branches` / `fork` / `rewind`（消息节点导航：uuid 闭环 + draft）
│   ├── asks.rs                      `wing asks`（在挂 Ask 的 tool_call_id 发现口：WS 快照 + `--wait`）
│   ├── tag.rs                       `wing tag`（会话标签读写 + 全局清单；幂等、不水合被逐出的会话）
│   ├── control.rs                   `wing interrupt`（别名 int）/ `compact` / `update`（会话控制面）
│   ├── lifecycle.rs                 `wing new` / `resume`（建空会话 / 显式水合）
│   ├── release.rs                   `wing release` 逐出会话内存态（显式 eviction，幂等）
│   ├── archive.rs                   `wing archive` 归档 / 恢复 / 列出归档区（一个子命令三形态）
│   ├── reload.rs                    `wing reload` 系统热重载（逐项结果按契约名字序）
│   ├── messages.rs                  `wing tail` / `wing head`（消息过滤，类 Unix head/tail）
│   ├── watch.rs                     `wing watch`（会话型唤醒：订阅收口 / 提问 → 叫醒我的会话）
│   ├── wake.rs                      `wing wake`（定时唤醒：in/at/every/daily + 时长与 spec 纯函数）
│   ├── query.rs                     `wing models` / `tools` / `agents`（查询端点，表格 / JSON）
│   ├── settings_api.rs              Setting API 的可注入门面（`SettingsApi`，config / setup 共用）
│   ├── wake_common.rs               `watch` / `wake` 共用的三形态分发（create / list / cancel）
│   ├── config/                      `wing config` 子命令族（mod / doctor / read / write / render / error / catalog / path / testkit）
│   └── setup.rs                     首次运行向导（无 session 的 setup 循环 + 三前端共用的配置预检）
├── acp/                             ACP 前端（wing acp，stdio 上的 Agent Client Protocol 服务端）
│   ├── mod.rs                       入口（ensure gateway → WS 连接 → HTTP client → 服务循环）+ CLI 参数
│   ├── agent.rs                     ACP handler 注册（initialize / new / prompt / cancel / list / load / resume / close / set_config_option）
│   ├── model.rs                     模型 config option（值域 / 分组 / currentValue）+ 热切换 + 外部变更中继
│   ├── session.rs                   SessionHub：会话表 · WS 事件泵与分流 · 挂载（load/resume）与回收（close）· prompt 串行化
│   ├── ask.rs                       ask 事件 → ACP 正式交互面（request_permission / elicitation 三径分流）
│   ├── translate.rs                 WingEvent → ACP session/update 映射（工具卡片状态 + prompt 拍平 + 终态判定）
│   └── translate/replay.rs          session/load 的历史回放投影（sync_session → update 序列）
├── stdio/                           headless 前端（wing -p，Claude 协议）
│   ├── mod.rs                       run_stdio + ensure_gateway_running + 参数过滤
│   ├── ndjson.rs                    stream-json NDJSON 帧
│   ├── renderer.rs                  text / json / stream-json 输出渲染
│   ├── stdout.rs                    stdout 串行化写口（renderer 与 stdin pump 共享）
│   ├── stream.rs                    `--include-partial-messages`：三路 delta → Anthropic SSE `stream_event`
│   ├── stdin_handler.rs             常驻 stdin pump（initialize / interrupt 应答 + 收尾）
│   └── test_support.rs              单测共用的事件 fixture 与渲染器装配（`#[cfg(test)]`，两个测试模块共用）
├── gateway/                         WS 客户端与分片重组
│   ├── client.rs                    GatewayClient — WS 连接 + 读写任务
│   └── chunk.rs                     `_chunk` 信封在读任务内重组（保序 / 有界 / 安全失败；应用层零感知）
├── protocol/                        WingEvent + ClientRequest + ConnectResponse（Python 事件的 Rust 镜像）
│   ├── events.rs / client_request.rs / connect_response.rs
│   ├── history.rs                   SessionMessage — 会话历史 Message 投影的 typed 镜像（media 宽容解析）
│   └── media.rs                     媒体 id 形态（64-hex 闸门）与上传前的预测引用（id = sha256）
├── shared/                          中立层：App 与 UI 共享的状态机与词汇（不依赖 app / ui）
│   ├── panels/mod.rs                选择面板内核（翻页 / 光标 / 窗口 / commit；存储归 adapter）
│   ├── panels/ask.rs                ask 模型与归一化入口（AskUserQuestion 面板 / Bash 确认的必选形态 / 只读提示）
│   ├── panels/picker.rs             /model 适配器（provider tab × model 行，Enter 即应用）
│   ├── panels/settings/             设置面板状态机（左栏分组锚点 · 右栏树扁平化 · 双栏焦点 · 内联编辑器 · 列表增删移 · 搜索 · 问题清单 · 键位与 Esc 阶梯）
│   ├── pinning.rs                   会话置顶（pin）约定：`pin` 标签 + 「置顶在前、后 pin 更靠前」的唯一排序实现 + 星标字形词表（后端仅在自动归档豁免处感知 `pin`）
│   ├── doc_edit.rs                  稀疏配置文档的路径代数与编辑原语（唯一实现；`wing config` 与设置面板各按自己的策略消费）
│   ├── text_edit.rs                 单行文本编辑原语（char↔byte 换算 + `EditOp`；设置/ask 面板与 composer 共用）
│   ├── tips.rs                      开屏提示池（欢迎屏轮换一条 + /tips 面板全量）
│   ├── session_tree.rs              会话血缘森林：扁平会话行 → 树序 + 连接前缀 + crew 聚合 + 层投影（TUI 面板 / `wing ps` / `wing usage --tree` 共用）
│   └── constants.rs                 协议常量（本地命令、工具名等 magic string）
├── app/                             App 状态机 + 事件循环
│   ├── mod.rs                       run_app() 主循环 + handle_event()
│   ├── runner.rs                    执行 AppIntent（HTTP/WS 副作用）
│   ├── intent.rs / transport.rs     AppIntent 枚举 + 传输抽象（WS+HTTP+client_id 原子单元，含重连退避）
│   ├── commands.rs / modal.rs       斜杠命令路由表 / 模态所有权与 Esc 阶梯（谁拥有键盘，唯一声明）
│   ├── mouse.rs / selection_session.rs   指针所有权与优先级（滚动条 → composer → chat 带 → 状态栏）/ 选择手势生命周期（区域指纹失效 · 按下时的链接）
│   ├── frame.rs / projection.rs     上一帧几何记录（输入路径共读的契约）/ WingEvent → chat·turn·status 投影（唯一 match 事件变体处）
│   ├── clipboard.rs                 粘贴通道边界（文本 → composer / 图片 → 单并发压缩流水线 → chip）
│   ├── images/                     图片 lane：mod（能力/配置门 · ImageStore 持有 · 元数据表 · 帧末绘制（遮挡与选择门）· 新鲜度检查（1s 节流 · 可注入时钟））· media（媒体图的字节侧：屏幕目标集 · 抓取账本 · 字节 LRU）
│   ├── replay.rs                    SyncSession 重放 → ChatCells（messages → events 能力分发）
│   ├── turn_state.rs / render_context.rs   轮次耗时 / 流式目标 cell 跟踪
│   ├── popup_state.rs               Popup + 候选缓存 + 去重 + 层视图状态（当前层的路径 / dormant 开关）
│   ├── preview.rs                   整屏帧取样器（创作期工具的库侧：真 App::draw → Buffer → 文本 / ANSI / HTML）
│   ├── settings.rs                  设置面板接线（开合生命周期 · Interface 实时预览 · 保存回执 · Ctrl+R 重启网关）
│   └── tests/                       模块内单测（mod + 18 个主题文件：core / send / wire / frame / interaction / modal / composer / commands / projection / selection / scrollbar / settings / images / clipboard / usage / welcome / splash_frame / program_status + support）
├── ui/                              UI 组件
│   ├── chat_view/                   Chat 视图：mod（ChatView 结构）· cell（ChatCell 渲染 + pending 条目）· model（内容模型）· media（消息里的图片：正文 + 引用 → 图块 / 标签行）· viewport（滚动·几何·高度缓存·绘制）· frame（帧快照·选择映射）· link（链接表·OSC8）· image（图片放置表与候选路径）
│   ├── selection.rs                 文本选择状态机（区域标签 / 内容坐标锚定 / 区间有序化 / 快照取文本，纯逻辑）
│   ├── scrollbar.rs                 overlay 滚动条（几何 / 命中测试 / 拖拽状态机 / 绘制）
│   ├── cached_cell.rs               ChatCell 包装：渲染结果 + 高度按 generation 缓存 + CellFrame 投影（链接 / 图片锚点侧信道）
│   ├── chip.rs                      带底色的小色块（键帽 / 徽章）：宽度（CJK 记 2 列）· 内边距降级 · 明暗两套底色与退路 · 带闸门的状态墨（开屏名牌、状态栏 pin 与模型段、输入卡的 Enter 键帽消费）
│   ├── emoji_width.rs               出帧前的宽度归一化（VS16 表情格子钉 ForcedWidth；每帧必跑，出帧口 tui::draw_frame 调用）
│   ├── html.rs                      Style → CSS / HTML 转义 / 页面底色（HTML 导出的唯一实现：theme_preview 与 splash_preview 共用）
│   ├── image/                       终端图形（唯一 door to ratatui-image/image）：probe（能力探测·可注入）· store（worker+LRU+epoch + 上限：文件/像素/缓存张数与字节/memo）· place（paint 原语）
│   ├── panel.rs                     选择面板共享渲染（窗口数学取自 shared/panels 内核）
│   ├── settings/                    设置面板浮层 overlay（卡片几何 / 左栏锚点 / 树 / 详情栏 / 编辑器 / 问题清单 / 帮助 / 提示，只读渲染）
│   ├── shimmer.rs                   扫光 / 混色原语（开屏 wordmark 与思考块标题行共用）
│   ├── welcome/                     开屏欢迎屏：mod（状态·宽度阶梯·可见性门控）· art（海鸥帧 + 像素大字数据）· sprite（半格渲染 + 品牌调色板）· motion（idle/干活动作规划）· wordmark（渐变 + 扫光）
│   ├── status_bar.rs / spinner.rs / toast.rs
│   ├── input_area/                  Composer 悬浮卡片（chrome：悬浮几何·活动栏·元信息栏 / model：段 + chip 引擎（粘贴 chip 与图片 chip 同一套原子语义）· 注册表捆包 Chips / image：图片 chip（载荷 / 号码 / 标签文案 / insert_image）/ widget / editing / movement / wrap / 指针映射与高亮 pointer / paste / helpers）
│   ├── popup/                       command（斜杠命令 + 候选项 + 会话面板的层投影 / chrome）/ selection（通用可选列表 + 会话富行：crew 徽章 · 层级装饰 · 面包屑与尾注）
│   └── cells/                       Chat cell 渲染（tool_call / thinking / todo_msg / ask_msg / diff_view / model_picker / notice）
├── render/                          Markdown + 语法高亮
│   ├── ansi.rs                      Style → SGR 的库里唯一实现（CLI 表格 / 热力图 / 主题画廊 / 渲染探针 / 整屏帧共用；每段自闭合。`welcome_preview` / `thinking_preview` 两个 example 各留一份窄编码器，不在此列）
│   ├── markdown/                    types / parsing / code_blocks / tables / links / wrap（CJK UAX#14）/ images（图片锚点）/ math（公式渲染）/ profile（Content vs Thinking 的规则归属）/ stream（StreamingRender：稳定前缀 + 活动尾部）
│   ├── picture.rs                   PictureSource — 一张图片的身份（本地文件 / 网关媒体对象）
│   ├── heatmap.rs                   日历热力图（7 行 × ISO 周的格子网格：四分位分档 / 月份标 / 宽度裁剪）
│   ├── table/                       共享表格引擎：列宽分配 / 列分类 / 框线皮肤（markdown 表格与 CLI 共用）+ plain（CLI 纯文本表格渲染，wing ps / tools 用）
│   ├── syntax.rs                    syntect 高亮（two-face 主题）
│   ├── diff_highlight.rs            diff 双修订版高亮（old/new 两路状态机：删除行→old，其余→new，context 行两路都要推进）
│   ├── fit.rs                       像素↔字符格共享装填（fit_cells；布局与编码同一份数学）
│   ├── test_support.rs              测试共用的行/链接断言助手（`#[cfg(test)]` 之外的轻量工具）
│   └── renderable.rs                CellContext（每帧渲染上下文：调色板 / thinking 模式 / 布局 / 图片选项）
├── tui/                             终端生命周期与终端通道（唯一出帧口 draw_frame）
│   ├── mod.rs                       init/restore、crossterm 事件流、draw_frame
│   ├── appearance.rs                OSC 11 背景探测（`colors.preset: auto` 的明暗判定；启动期一次、150ms 超时）
│   ├── clipboard_protocol.rs        OSC 52 读写协议（唯一 id 让被放弃的读不落到下一次）
│   ├── cursor.rs                    光标形状 / 可见性
│   └── stdin_proxy.rs               stdin 接驳（在 crossterm 之前安装的读代理；session_guard 的靶子）
├── config/                          TUI 配置（mod=配置模型 + 调色板 preset/appearance / colors / rendering / paste=粘贴图片压缩参数 / catalog=Interface 根声明 + 规范形 dump / store=读写+指纹+原子写）
└── util/                            clipboard / clipboard_paste（本地剪贴板读取）/ base64（RFC 4648 编解码，OSC 与剪贴板协议共用）/ mime（MIME 归一）/ format（token / 命中率 / 图片标签等共享记法）/ image_compress（粘贴图片压缩）/ open(链接打开) / logging / osc9（桌面通知）/ program_status（OSC 7501 程序状态）/ partial_json / title（OSC 0）/ wing_home（WING_HOME 解析）
```

配套：`crates/wing/benches/`（criterion 基准：流式渲染 `stream_render` · 工具参数流式解析 `tool_args_stream` · 图片逐帧与编码 `image_frame` · 大会话重放与首帧 `session_replay`）、`crates/wing/tests/`（测试二进制 + `common/` 与 `acp_harness/` 支撑模块，五类：渲染回归 `stream_render_*` · `math_render` · `markdown_math_drift` · `vs16_row_drift`；前端形态 e2e `acp` · `control_plane` · `usage` · `stdio_resident` · `stdio_one_shot` · `tail_filter` · `setup_preflight` · `wait_fail_fast` · `status_build_skew` · `wing_home` · `session_lineage`；stdin 接驳与终端误解析 `stdin_proxy_*` · `crossterm_*` · `tui_event_stream_*` · 终端能力探测 `appearance_probe_pty`；WS / argv / 退出摘要 `ws_*` · `argv_non_utf8` · `exit_summary_shape`；源码门禁 `layer_guard` · `session_guard`；共享底座 `common/{e2e,guard,pty}`）、`crates/wing/examples/`（8 个：`export_logo` 从欢迎屏像素网格导 SVG · `welcome_preview` 开屏预览 · `splash_preview` 开屏整块（状态栏 + 牌子 + 输入卡）的真帧 · `render_probe` markdown 渲染探针 · `theme_preview` 主题预览 · `reconnect_flow_verify` 网关重启后的重连流程验证 · `image_demo` 单图走 `ui::image` 真实管线（终端兼容性矩阵的肉眼验收）· `thinking_preview` 思考行创作期预览（`--animate` 看刷光））。

源码门禁：`layer_guard.rs` 把 Rust 侧分层规则钉成扫描断言（Python 侧对应 `libs/core/tests/test_layering.py`，两侧规范见 [docs/dev/backend-layout.md](docs/dev/backend-layout.md)）；`session_guard.rs` 钉**编译器管不着的终端时序**（接驳安装点唯一、settle 早于首次 poll 等五条规则，语义见 [docs/dev/tui-input.md](docs/dev/tui-input.md)）。

### 其他

- `crates/wing-api-client/src/` — 手写 Rust HTTP 客户端：`client.rs`（全部 API 方法）、`models.rs`、`error.rs`、`tool_host.rs`（远程工具宿主，WS 服务循环 + builder）。
- `crates/wing-math/` — LaTeX 数学子集 → 终端字符网格（借鉴内联 `term-maths` + `rust-latex-parser`，附出处/许可）：窄接口 `render_inline` / `render_display` / `render_block`，`None` = 「不该由引擎渲染，请显示源码」。接线（事件、定界符归一化、降级）在 `crates/wing/src/render/markdown/math.rs` → [docs/dev/tui-rendering.md](docs/dev/tui-rendering.md) 第二节·五。
- `libs/wing-sdk/wing_sdk/` — Python 远程工具宿主 SDK：`host.py`（装饰器注册 + WS 循环）、`http_client.py`、`schema.py`、`tools/`（Bash/Read/Write/Edit/Glob/Grep，workspace-bound）。
- `assets/` — 品牌与演示素材（README 页头 banner 明暗两版、站姿 mascot SVG、社交预览 PNG、README 的 demo/速度 GIF）：SVG 由 `examples/export_logo.rs` 从欢迎屏的同一份像素网格导出，README 的 GIF 由 `scripts/demo/`（假 Provider 喂真 TUI，`make demo`）录制后挂在 `readme-assets` rolling release 上（不进 git），性能数字由 `scripts/demo/latency.py` 现量 —— 不会漂移 → [scripts/demo/README.md](scripts/demo/README.md)。
- `libs/wing-probe/` — 确定性集成测试基础设施（假 Provider + driver + observer 断言库）：`wing_probe/`（env / provider / driver / watch / history / files / toolhost）、`scenarios/`（整机断言场景）、`tests/`（基础设施自测）。**禁止 import `wing`**（AST 门禁强制；允许 `wing_sdk`），一切经公开 HTTP / WS 协议 → [docs/dev/probe-testing.md](docs/dev/probe-testing.md)。
- `scripts/perf/` — PR 级性能回归测量（merge-base 交错 A/B：criterion 子集 / TUI 显示延迟 / gateway socket 套件 → sticky comment；`.github/workflows/perf.yml` 在 PR 上跑并评论，纯报告不做门禁）→ [docs/dev/perf-testing.md](docs/dev/perf-testing.md)。
- `extensions/vscode/` — VSCode 前端（编辑器内的接入面，与 `wing` 二进制的四种形态并列；TS strict + pnpm 单包四层：`src/core` 网关能力层 / `src/host` 扩展宿主 / `src/webview` React 渲染 / `src/shared` 两侧契约）。层门禁由机制强制：分 tsconfig（DOM/node 隔离）+ ESLint 分区规则 + `tests/layers` 守门测试；`make check`/`make test` 含 `check-ts`/`test-ts`，CI 有 `typescript-check` job → [docs/dev/vscode-extension.md](docs/dev/vscode-extension.md) · [extensions/vscode/README.md](extensions/vscode/README.md)。
- 测试目录：`libs/core/tests/`（后端 pytest，130+ 个测试文件 + `conftest.py`（fixture）+ `_support.py`（跨文件 helper））、`libs/wing-sdk/tests/`。
- 顶层 `docs/dev/` 为开发者深度文档（中文），`scripts/sync_version.py` 同步版本号。

## 配置与日志

`$WING_HOME`（默认 `~/.wing`）目录布局、`config.yaml` 顶层键、前后端统一的日志策略与按日期 grep 技巧 → [docs/dev/config-logging.md](docs/dev/config-logging.md)。

## 深潜阅读（docs/dev）

AGENTS.md 保持高信息密度总览；机制级细节去 `docs/dev/`（中文）：

| 文档 | 内容 |
|------|------|
| [`docs/dev/architecture.md`](docs/dev/architecture.md) | 三层架构与数据流、TUI / stdio / ACP / 编排 CLI 四种前端形态、远程工具、会话生命周期与中断提交语义、事件系统与统一日志、持久化与压缩 |
| [`docs/dev/backend-layout.md`](docs/dev/backend-layout.md) | 后端分层规范（`libs/core/wing/**`）：分层图与依赖方向、每包职责一句话、迁移映射（历史记录）、分层守门测试（`test_layering.py`） |
| [`docs/dev/cache-accounting.md`](docs/dev/cache-accounting.md) | 缓存账本（KV 前缀缓存）：分段表（谁产 label）、账本位置与上界、理论命中率定义、verdict / blame / avoidable 全表（每条结论落到「我能避免 / 该报 bug / 什么都不用做」）、降级与排错手册、回滚 |
| [`docs/dev/usage.md`](docs/dev/usage.md) | 用量统计：数据来源（`audit_*` + `session_catalog`）、`GET /api/usage` 契约与各维度、11 个度量的口径与可归因性、一天与时区、根/子会话软机制、`wing usage` 命令面（热力图分档 / 7 个 `--by` / 仪表盘五段）与 `wing info` 的用量块、与 `wing cache` / 会话账本的口径差异、回填与历史起点、增长与保留、已知缺口、排错 |
| [`docs/dev/http-api.md`](docs/dev/http-api.md) | 完整 HTTP 端点表 + WebSocket 协议 + 鉴权 |
| [`docs/dev/glossary.md`](docs/dev/glossary.md) | 核心概念速查：SessionStore / MessageLog / TrackedList、工具命名空间、prompt 命令、压缩等 |
| [`docs/dev/config-logging.md`](docs/dev/config-logging.md) | WING_HOME 布局、config.yaml 键、日志轮转与查询 |
| [`docs/dev/settings.md`](docs/dev/settings.md) | 设置面：一份声明生成目录/模板/校验、业务分组（界面分类的唯一声明处）、稀疏文档与「缺席即默认」、Setting API（4 端点 + 保存事务 + 密文语义 + 生效域全表）、setup mode（降级启动 / loopback-only 修复）、TUI 浮层面板（双栏 + 键位表）、首次运行向导、`wing config` CLI |
| [`docs/dev/media-images.md`](docs/dev/media-images.md) | 媒体与图片（read-image）：ReadImage 工具、内容寻址媒体池、模型能力声明、请求期图片投影（高水位 + 量子批量驱逐）与 KV/前缀 cache、inline/followup 线格式、probe 场景清单 |
| [`docs/dev/tui-rendering.md`](docs/dev/tui-rendering.md) | TUI markdown 渲染：`render_probe` 调试入口、Content/Thinking 两个 profile 的差异、公式（`$…$` / `$$…$$` / AMS 环境）渲染与定界符归一化、流式静息态 == 参考渲染的不变量、已知边界、图片锚点的行数纯函数与路径策略 |
| [`docs/dev/tui-theming.md`](docs/dev/tui-theming.md) | TUI 明暗主题：`colors.preset` 三值（auto/dark/light）、显式 `appearance`、light 调色板与 WCAG 判据、OSC 11 探测与降级、浅色下各渲染面的处置（toast/选中行/品牌图形/语法主题） |
| [`docs/dev/tui-images.md`](docs/dev/tui-images.md) | TUI 图片能力：两档阶梯（可渲染 / 标签）、探测与配置、三态、资源上限与压力验证、**新鲜度**（重写同一路径 ≤1s 换图）、**用户消息里的图**（媒体对象：形状来自引用、按视口抓字节、字节 LRU、失败即状态）、失效触发点、遮挡与选择、性能数字、真机验收清单、症状→先看哪里 |
| [`docs/dev/vscode-extension.md`](docs/dev/vscode-extension.md) | VSCode 扩展（`extensions/vscode/`）：四层分层与数据流、桥协议与归约（重放==直播 / 单 WS 多订阅）、会话时序与多 Tab、连接自愈、构建门禁 / smoke / 打包与验收 |
| [`docs/dev/wake.md`](docs/dev/wake.md) | 会话唤醒：语义表（收口/提问/失败/归档联动）、时间语义与重启后的错过补偿、内置信封逐字形状、HTTP / CLI、后端包结构与已知边界 |
| [`docs/dev/session-lineage.md`](docs/dev/session-lineage.md) | 会话血缘：父子关联字段语义、采集机制（CLI / TUI / 外部前端）、树序与 pin 规则的产品语义参照点 |
| [`docs/dev/tui-input.md`](docs/dev/tui-input.md) | TUI 输入通道：鼠标上报的 DECSET 配对与生命周期对称、滚轮 / 键盘 / 选择的路由（新增鼠标能力前先读） |
| [`docs/dev/tui-signals.md`](docs/dev/tui-signals.md) | TUI 终端状态信号：OSC 0（标题）/ OSC 9（桌面通知）/ OSC 7501（程序状态）的读者与语义、光标转义的变化语义 |
| [`docs/dev/coverage.md`](docs/dev/coverage.md) | 覆盖率门禁：增量（patch）覆盖率作为 PR 门禁、sticky 评论形状、不判定区与本地跑法 |
| [`docs/dev/probe-testing.md`](docs/dev/probe-testing.md) | 确定性集成测试（wing-probe）：跑法 / 新增断言场景（写代码、不写配置）/ 断言原语速查 / 上下文红线清单与 persist 口径 / 逃生舱约定 / 停机两路径（快 `stop()` vs 优雅 `shutdown()`） |
| [`docs/dev/perf-testing.md`](docs/dev/perf-testing.md) | PR 级性能回归测量（perf-ci）：方法论（merge-base 交错 A/B / 三层测量）、四个套件与指标表、本地跑法与产物、加套件/指标/bench 的约定、阈值与校准口径（初始值与依据）、CI 行为与局限、排查手册 |
| [`docs/dev/test-speed.md`](docs/dev/test-speed.md) | 测试回环的成本结构：四组的并行度旋钮（独占 / 共享档）、各组时间花在哪（probe 的网关启动、python 的真实等待、rust 的份额分线程、ts 的 jsdom）、改测试时的判据、`scripts/bench_tests.py` 的交错 A/B 与负载门禁 |
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
make test                         # 只跑测试：Python + Rust + TS + probe（四组并行，共享档见 docs/dev/test-speed.md）
make test-probe                   # 确定性集成场景（wing-probe，离线、无外部 API key）
make check                        # 只跑静态检查：Python + Rust + TS（含 extensions/vscode 门禁）
make fmt                          # 格式化全部
```

**后端特性测试约定**：开发后端特性（新增 / 修改 `libs/core` 的行为——上下文链、事件、协议、工具、网关等）时，**必须在 `libs/wing-probe/` 下增加真实有效的测试**：断言场景用代码写（`scenarios/`，不写配置文件），经公开 HTTP / WS 协议驱动"真网关 + 假 Provider"；`make test-probe` 与 CI 的 `probe-check` job 会强制其通过。**上下文红线**（compact / rewind / fork 等一切对上下文的操作）的行为变更必须配套红线断言。详见 [docs/dev/probe-testing.md](docs/dev/probe-testing.md)。

**注释纪律**：注释**宁缺毋滥**，只承载代码本身读不出的信息——不变量、协议 / 契约、红线、非显然的 why（平台差异、竞态、外部约束）。保持**高信息密度**：函数 / 类型名已说清的不复述，同一主题的连续多行注释压成要点式，与代码不符的过期注释直接删。**严禁**添加「解释自己这处改动为什么正确」的辩护式注释（"原来 XX 会出问题"、"这样改是对的"、"按评审意见修成…"）——改动理由写进 commit message / PR 正文。

`wing` 命令面（四种形态 / 命令表 / 编排脚本 / 红线 / 常见坑）→ [`.agents/skills/wing-cli/SKILL.md`](.agents/skills/wing-cli/SKILL.md)。

### Worktree 纪律

Rust 的构建产物是**按路径独立的一整套**：新建 worktree 后首次构建会凭空造出一整套 `target/`；
本地若还跑插桩覆盖率，会再多一整套 `target/llvm-cov-target/`。并行开几个 lane 就足以把磁盘吃满。

- **建**：用 `scripts/dev/new-worktree.sh <name> [base]`，不要裸 `git worktree add`。
  它从主仓克隆 `target/debug`：在支持写时复制的文件系统（APFS / btrfs / XFS）上物理占用≈0，
  外部依赖免重编（工作区自己的 crate 因路径变化仍需重编），磁盘只付差异。
- **收**：任务交付即 `git worktree remove --force <path>`。不要过夜、不要囤着等下一轮 review。
- **收尾进程**：从 worktree 里起过 `wing-gateway` / dev server 的，交付前确认已退出
  （`lsof -nP -iTCP -sTCP:LISTEN`）。一台机器上只应存在一个网关。
- **本地不跑插桩覆盖率**：`make test-rust-cov` 会留一整套 `target/llvm-cov-target/`，
  本地日常用 `make test`（普通构建）即可，覆盖率交 CI 的 `rust-check` job。
- **不依赖 sccache**：开发循环已经用它不上了。若 `~/.cargo/config.toml` 还挂着
  `rustc-wrapper = "sccache"`，建议摘掉 —— 它不会让构建失败，但增量编译开着时本地
  crate 的编译进不了缓存，而 `CARGO_INCREMENTAL=1` 这种临时覆盖会让 sccache 直接报错退出。

## 分发

- **Python**：`pip install wing-agent` → 安装 `wing-gateway`（Python 网关）与 `wing-cli`（maturin 构建的 Rust 二进制，提供 `wing` 命令）。
- **Rust**：GitHub Release 预编译二进制 → `wing` CLI（TUI + stdio + 守护进程控制）。
- **SDK**：`wing-sdk` 为 uv workspace 包（`libs/`），未发布 PyPI。

## 交付终态

能力交付 = **推送 + 开 PR + 等云端 review 结束 + 派执行者修完意见**：

- 云端 review **只触发一次**，不因返修补提交而重复触发；
- 调度方不需要用户二次确认即可推送与开 PR（**合并仍由用户决定**）；
- 执行者只 commit，不推送。

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
