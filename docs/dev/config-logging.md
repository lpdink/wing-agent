# 配置与日志

## 目录布局（WING_HOME）

`WING_HOME` 覆盖 `~/.wing`；后端数据统一落在 `$WING_HOME/core`（`wing/config/loader.py::get_wing_home()`）。`WING_SESSIONS_PATH` 额外覆盖 sessions 目录。

两端的取值口径必须一致（`libs/core/wing/config/loader.py::get_wing_home()` ↔ `crates/wing/src/util/wing_home.rs`）：**空串视同未设置**（回落到 `~/.wing`）、**前导 `~` 展开为家目录**（后端 `Path.expanduser()`；`~user` 是唯一不展开的写法——std 取不到 getpwnam），且路径**按字节原样使用**——非 UTF-8 的 home 是真实目录（Python 用 surrogateescape 拿到同一串字节），前端不得静默回落到 `~/.wing`，否则配置 / 日志 / 会话会分到两个 home。

```
~/.wing/
├── core/
│   ├── config.yaml     后端配置（唯一事实来源；缺失时由声明生成的模板创建——见下）
│   ├── config.yaml.bak 最近一次保存前的原文（覆盖式，只留一份；损坏的文件也逐字留证）
│   ├── logs/           后端日志
│   │   ├── wing_YYYY-MM-DD.log            网关运行时日志（按本地日期，append；恒 DEBUG）
│   │   ├── new.log → wing_YYYY-MM-DD.log  指向活跃后端日志的符号链接（仅后端；每次轮转 / setup 刷新）
│   │   └── gateway.log                    网关守护进程 stdout/stderr（uvicorn 错误、traceback；append）
│   ├── sessions/       会话持久化（metadata + history.jsonl + aux / metrics.json）；.media/ 为跨会话共享的图片媒体池（内容寻址）
│   └── metrics.json    全局指标（LLM / 工具调用 / 压缩，按天聚合）
└── tui/
    ├── config.yaml     TUI 配置（colors / layout / rendering / api_key；`wing tui --dump-config` 输出它的规范形，密文默认掩码）
    ├── config.yaml.bak 最近一次保存前的原文（同上）
    └── logs/           TUI 日志：wing_YYYY-MM-DD.log（命名同后端，无符号链接）
```

## 后端 config.yaml

字段元信息的**唯一来源**是 `wing/config/models.py` 的 `S(...)` 声明（`config/spec.py` 的声明层）；
规范形模板 / 保存写盘 / 面板 / CLI 全部由它生成——**手写模板 `config/default_config.py` 已删除**，
改字段只需改声明（门禁测试会迫使模板 / 目录 / 校验同步，见 [settings.md](settings.md)）。
每个字段的声明维度：`doc`（一行摘要 + YAML 注释）/ `notes`（多行详解）/ `title` / `apply`（生效域，
四档 `hot` / `next_session` / `restart` / `readonly`）/ `secret`（只写不回显）/ `choices`（枚举含义）/
`example` / `editable` / `section`（+`section_doc`）/ `summary_fields` / `min_items`；必填 = 无默认值。

文件是**稀疏文档**：缺席的键跟随声明默认值（emitter 把它们写成**注释掉的行**，想钉住就取消注释）；
「复位为默认」= 从文件里移除该键。保存经 `POST /api/settings/set`（唯一写盘路径）：
校验（全有或全无）→ 备份 `config.yaml.bak` → 规范形 YAML 原子写 → 热重载。
schema 之外的未知键**保留并写回**（前向兼容）。

顶层键（默认值与约束以 `config/models.py` 的声明为准；`wing config list` 可列全树）：

| 键 | 说明 |
|----|------|
| `providers` | LLM provider 列表（**必填非空**）。每项声明 `protocol: openai \| anthropic`、`base_url`、`api_key`，以及超时（`timeout_first_chunk` = **响应头**超时、`timeout_total` = 总时长；另有硬编码 120s 的**响应体停滞**判定，见 `provider/transport.py` 的 `STREAM_IDLE_TIMEOUT`——响应头到达后两次读取间隔超过它即判停滞并走 `with_retry`）、重试（`max_retries` / `max_retry_delay`）、`explicit_cache_mode`、`reasoning_effort`、`extra_body` 等；Anthropic 需 `max_tokens` / `anthropic_version`。`models`（**必填非空**）= 模型目录的**唯一来源**（远端 `/models` 发现已退役），三种声明形态：字符串 `- dfmodel`（id = name，存量零改动）/ `name: dfmodel-2026`（id = name）/ `id: ds-flash` + `name: dfmodel-2026`（显式 id = 全局唯一**引用词**，`agents[].model` / 协议 / metadata 引用它，id 缺省 = name）；可选 `display_name`（展示名，缺省前端回落 name）/ `description` / `capabilities: {vision: true}`（未声明 = text-only，键名拼错静默忽略）。**id 跨 provider 全局唯一**（冲突即加载失败，错误含修复示例）；同 provider 内 `name` 唯一。图片相关：`image_delivery`（`inline \| followup`，缺省按协议：openai → followup、anthropic → inline）、`image_max_bytes`（单图请求期兜底，见 [media-images.md](media-images.md)） |
| `agents` | Agent 模板列表（**必填**）：`model`（引用 `providers[].models` 的 **id**；配置加载期校验，查不中即失败并列出 available ids + 调用名提示）、`default`、`system_prompt`、`tools`、`context_window_tokens` / `keep_recent_tokens`、`skills` / `rules` glob。`provider` 字段已删除——写了被**静默忽略**（pydantic `extra="ignore"`，不报错、不 warning） |
| `hooks` | Hook 文件 glob |
| `safe_command_patterns` | Bash 自动放行的正则（白名单外的命令默认拦截） |
| `yolo` | 完全跳过危险命令审查 |
| `steer` | steer 模式开关 |
| `tool_result_truncate` | 超长工具结果截断（`max_length` / `keep_chars`，头尾保留 + 全文落临时文件） |
| `images` | 图片读入与请求期保留预算（`max_bytes` 4718592（4.5 MiB）/ `max_images` 32 / `count_quantum` 8 / `request_budget_bytes` 37748736 / `evict_quantum_bytes` 18874368，全部 > 0）——读图链路与投影算法见 [media-images.md](media-images.md) |
| `log.level` | 网关**控制台**级别（守护进程 stdout/stderr，被 `gateway.log` 捕获）保存即热生效；每日文件日志恒为 DEBUG |
| `gateway` | `host` / `port`（**restart 域**：改完需重启网关，不做假热更）/ `remote_tool_timeout` / `auth`（opt-in API key：`enabled` + `keys[{key, role}]`，角色 `admin` / `tool_runtime`；保存即热生效） |
| `commands.paths` | prompt 命令（`/xxx` 展开）的 glob 列表，每个 .md（frontmatter: name / description / aliases，正文 `$ARGUMENTS` 占位）定义一个命令 |
| `user_agent.preset` | HTTP User-Agent 预设（`opencode` / `qwen-code`） |
| `sessions` | `eviction`（空闲会话逐出：`enabled` / `idle_ttl_seconds`（默认 1800）/ `sweep_interval_seconds`（默认 300，启动时读取））。**存储路径不是配置字段**：`WING_SESSIONS_PATH` env > `$WING_HOME/core/sessions` |

> 改一个键什么时候生效（哪些要重启网关）都写在字段声明里（`apply`），面板行与
> `wing config list` 会显示；完整全表见 [settings.md](settings.md) 的「生效域」一节。

## TUI 配置（`~/.wing/tui/config.yaml`）

`colors`（`preset` = `wing` | `terminal` 选底色：`wing` 是给暗底终端设计的一套 hex 灰阶 + cyan accent（默认），`terminal` 跟随终端自己的 ANSI 色；其余每个槽位键**覆盖该槽**：`accent` / `text` / `thinking` / `tool_result` / `dim` / `success` / `warning` / `danger` / `math` / `surface` 与 diff 行背景 tint（`diff_add_bg` / `diff_del_bg` 及词级强调 `diff_*_bg_strong`），命名色或 24-bit hex 都行，非法值 warn 后回落该预设槽位）、`layout`（输入区 / 弹窗 / 工具输出的行数上限）、`rendering`、`api_key`（网关鉴权，空则不发送；**密文**——面板只写不回显、`--dump-config` 默认掩码，见下）。调色 / 对色用 `cargo run -p wing --example theme_preview`（整条 transcript 的真 cell 画廊，`--preset` 切预设、`--html` 导出）。

`rendering` 的键：

| 键 | 取值 | 默认 | 说明 |
|----|------|------|------|
| `thinking` | `visible` \| `hidden` | `visible` | reasoning 块的**默认呈现**：`visible` = 默认展开（标题行 + 正文）；`hidden` = 默认折叠（只剩标题行 `⦁ 深度思考中 4s`，持续刷光、完成后定格时长）。标题行是所有思考块的固有部分，`Ctrl+O` 全局切换详细 / 简略（所有轮一起、会话内保持），语义见 [`tui-rendering.md`](tui-rendering.md) 第二节·六 |
| `math` | `text` \| `off` | `text` | `text` = `$…$` / `$$…$$` / 裸 AMS 环境渲染成字符网格（渲染不了时显示完整 LaTeX 源码）；`off` = 完全不解析、不归一化，即未引入公式渲染前的行为。非法值 warn 后回落 `text` |
| `images` | `off` / `auto`（大小写不敏感） | `auto` | markdown 本地图片：`auto` = 启动时探测终端图形协议（kitty/sixel/iTerm2），支持就画真图；`off` 或探测失败 = 今天的链接路径（不探测、不读盘、零开销） |

非法的 `rendering.images` 值回退到 `auto` 并在 TUI 日志里 warn 一行；路径策略（workspace 相对 / 越界 / 远程 URL 一律退回链接路径）见 [`tui-rendering.md`](tui-rendering.md) 第五节，能力阶梯 / 资源上限 / 失效触发点 / **文件重写的新鲜度检查（1 s 窗口）** / 性能数字见 [`tui-images.md`](tui-images.md)。`math` / `thinking` / `images` 三个键都**大小写不敏感**：`Serialize` 写的是变体名（`Text` / `Off` / `Hidden`），`Deserialize` 两种拼写都认，dump → 改 → 回填不会把开关悄悄改回默认。diff 的上下文行数不是前端配置——窗口由后端随载荷下发（见 `diff-payload-window`），前端按给定内容逐行渲染。

**读写路径**：`crates/wing/src/config/store.rs` 是 `~/.wing/tui/config.yaml` 的唯一 I/O——
稀疏文档读取（只有文件里写下的键）+ 文件字节指纹（FNV-1a 64，`fnv1a64:<hex>`；文件不存在 = `"absent"`）+
原子写（tmp + `sync_all` + `rename`）+ 写前 `.bak`（覆盖式；**同样是原子写**：读旧文件字节 →
同一套 tmp + rename，不再用会留下截断副本的 `fs::copy`）。**读失败不吞**：文件坏掉时报错，
不静默回落默认值（否则一次保存就会覆盖掉用户手写的文件）。

编辑入口有三个，共用同一份声明（`config/catalog.rs::interface_catalog()`）：

- **TUI `/settings` 面板的 Interface 根**：实时预览（改颜色当场变色，`Esc` 回退）+ `s` 保存；
- **`wing tui --dump-config`**：读**当前文件**、输出它的规范形（注释来自声明、缺席的默认值写成注释行）；
  文件坏掉时 stderr 报错 + 非零退出（不假装一切正常）。
  - **密文默认掩码**（本 PR **改变了它的语义**：旧实现打印的是 `AppConfig::default()` —— 一份
    `api_key` 恒为空的默认模板；现在打印的是**当前文件**，所以必须管住文件里的密文）：
    secret 叶子输出 `api_key: null` + 一行 `# 已掩码（•••••••• 1234）`，用的是与设置面板 /
    `wing config get` **同一套** `•••••••• 末 4 位` 视觉语言（末 4 位只在值长度 ≥ 8 时给）。
    真值要显式加 **`--show-secrets`**（round-trip 用途：`wing tui --dump-config --show-secrets > f`，
    之后 `改写 f; 回填; dump --show-secrets > f2; diff f f2` 才是可验证的 round-trip）；
    单独传 `--show-secrets`（不带 `--dump-config`）会被拒绝，不静默忽略。
  - 为什么默认掩码：与项目「密文只写不回显」的整体姿态一致（后端 `get` 对密文恒返回 `null`，
    真值只以末 4 位 hint 出现），而 stdout 会被 shell 重定向、被日志与 CI 捕获；真值只在显式要时才给。
    代价是**不带 `--show-secrets` 的 dump 不是 round-trip artifact**（`api_key` 变成 `null`），
    这一点是刻意的。
  - **保存路径不受掩码影响**：`store::write_interface_doc`（面板 `s` / setup 向导 / `app/runner.rs`
    共用的唯一写盘实现）用的是同一个 emitter 的 **Raw** 模式——写掩码 = 把用户密钥换成掩码字符串
    = **数据损坏**。两处别"顺手统一"。
  - `--show-secrets` 的输出是真值：`> 已存在的文件` 前先备份（shell 会在程序启动前先截断目标）；
- **手写文件**（仍然支持）：外部改动在面板 `R` 重新载入时读入。

> 颜色槽「注释掉的默认值」是 `preset: wing` 下的值；改 `preset` 后它们仅供参考（不随预设重算）。
> 注释行的"取消注释"只对**单行标量**成立；嵌套块（`colors:` 等）请用面板的"复位为默认"逐项调整。

> `gateway.host/port` 只影响独立启动 `wing-gateway` 的场景；Rust TUI 读的是 backend config，不会读 TUI config 里的网关地址。

## 日志策略（前后端统一）

- 双方都按**本地日期**一天一个文件：`wing_YYYY-MM-DD.log`，append 模式打开——网关 / TUI 重启绝不截断或分裂日志；启动与每次轮转时 prune 7 天前的文件（含遗留命名）。
- 后端日志在 `~/.wing/core/logs/`（`new.log` 始终指向活跃后端日志）；TUI 日志在 `~/.wing/tui/logs/`（命名相同，**无**符号链接）。
- 日志初始化是显式的：网关 CLI（`wing-gateway` → `wing.common.logger.setup_logger`）与 `wing` 二进制（`cmd::dispatch` 入口统一初始化，TUI / stdio / 全部编排子命令共用同一份 `util/logging.rs`，幂等）在启动时挂 handler。**import `wing` 没有任何日志副作用**——测试与脚本永远不会在 `~/.wing` 创建文件。
- 网关 lifespan 里把 asyncio 未处理异常的兜底接进 wing 日志：`asyncio unhandled: message=… task=…`（带 traceback），随后原样转发给原处理器（stderr 行为不变）——「Task exception was never retrieved / Task was destroyed but it is pending」这类暗角不再只沉在 `gateway.log`（`common/logger.py::install_loop_exception_logger`）。
- 后端每行格式 `YYYY-MM-DD HH:MM:SS - LEVEL - [<session_id> <request_id>] - path:line - message`；TUI 由 tracing 输出（本地时间，`RUST_LOG` 可覆盖级别，默认 `wing=warn`）。
  - **关联段**逐条从协程上下文（`request_context.RequestContext`）读取——接线在组合根（`gateway/cli.py` 把 `get_request_context` 传给 `setup_logger`，formatter 不依赖领域类型、`common` 保持 L0）；只有一个 id 时只标注一个，两个都没有时整段省略。turn 全链路（provider / 工具 / ReAct / 压缩 / `SM._post` / turn 内 hook）自动带上；会话逐出拆解（`_teardown`）、WS 帧处理、`/api/session/compact` 显式绑定（`request_context.session_context`）。**事件循环兜底日志**（`asyncio unhandled: …`）刻意不标注：它在 GC 时机触发，当前上下文可能是任意无关任务（错误归属比不归属更误导，见 `common/logger.py` 的 `NO_CORRELATION`）。追一个会话直接按 id grep：

```bash
grep 20261009-223452-932fd6d1 ~/.wing/core/logs/new.log
```

  - **路径字段**：包内文件相对 **wing 包的父目录** 渲染——dev（`libs/core`）与安装态（site-packages）都得到 `wing/provider/openai/provider.py:328`；包外文件（`~/.wing/hooks/*.py` 等）回退绝对路径——那是唯一能定位它们的方式。
  - 控制台 handler **只在真正的终端上色**：守护进程 stdout 落 `gateway.log`（非 tty）时不再混入 ANSI 色码。
- 前后端一律使用本地时间，时间范围 grep 可直接工作：

```bash
grep '^2026-09-08 23:' ~/.wing/core/logs/new.log
awk '$0 >= "2026-09-08 23:10" && $0 < "2026-09-08 23:30"' ~/.wing/tui/logs/wing_2026-09-08.log
```

## 环境变量

| 变量 | 作用 |
|------|------|
| `WING_HOME` | 覆盖 `~/.wing`（后端数据在 `$WING_HOME/core`，TUI 在 `$WING_HOME/tui`）；空串视同未设置、前导 `~` 展开、取值按字节原样使用（见上「目录布局」） |
| `WING_SESSIONS_PATH` | 覆盖 sessions 目录 |
| `RUST_LOG` | TUI tracing 级别（默认 `wing=warn,tokio_tungstenite=warn`） |
