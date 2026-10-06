# 配置与日志

## 目录布局（WING_HOME）

`WING_HOME` 覆盖 `~/.wing`；后端数据统一落在 `$WING_HOME/core`（`wing/config/loader.py::get_wing_home()`）。`WING_SESSIONS_PATH` 额外覆盖 sessions 目录。

```
~/.wing/
├── core/
│   ├── config.yaml     后端配置（唯一事实来源；缺失时由 wing/config/default_config.py 模板生成）
│   ├── logs/           后端日志
│   │   ├── wing_YYYY-MM-DD.log            网关运行时日志（按本地日期，append；恒 DEBUG）
│   │   ├── new.log → wing_YYYY-MM-DD.log  指向活跃后端日志的符号链接（仅后端；每次轮转 / setup 刷新）
│   │   └── gateway.log                    网关守护进程 stdout/stderr（uvicorn 错误、traceback；append）
│   ├── sessions/       会话持久化（metadata + history.jsonl + aux / metrics.json）；.media/ 为跨会话共享的图片媒体池（内容寻址）
│   └── metrics.json    全局指标（LLM / 工具调用 / 压缩，按天聚合）
└── tui/
    ├── config.yaml     TUI 配置（colors / layout / rendering / api_key）
    └── logs/           TUI 日志：wing_YYYY-MM-DD.log（命名同后端，无符号链接）
```

## 后端 config.yaml

顶层键（手写注释模板即 `wing/config/default_config.py`，改 Config 字段时须同步维护）：

| 键 | 说明 |
|----|------|
| `providers` | LLM provider 列表（**必填**）。每项声明 `protocol: openai \| anthropic`、`base_url`、`api_key`，以及超时（`timeout_first_chunk` = **响应头**超时、`timeout_total` = 总时长；另有硬编码 120s 的**响应体停滞**判定，见 `provider/transport.py` 的 `STREAM_IDLE_TIMEOUT`——响应头到达后两次读取间隔超过它即判停滞并走 `with_retry`）、重试（`max_retries` / `max_retry_delay`）、`explicit_cache_mode`、`reasoning_effort`、`extra_body` 等；Anthropic 需 `max_tokens` / `anthropic_version`。`models` 静态列表跳过远端查询，元素为字符串（存量）或对象（`name` = 实际调用名 / `display_name` / `description` / `capabilities: {vision: true}`；未声明 = text-only，键名拼错静默忽略）。图片相关：`image_delivery`（`inline \| followup`，缺省按协议：openai → followup、anthropic → inline）、`image_max_bytes`（单图请求期兜底，见 [media-images.md](media-images.md)） |
| `agents` | Agent 模板列表（**必填**）：`model`（+ `provider` 引用）、`default`、`system_prompt`、`tools`、`context_window_tokens` / `keep_recent_tokens`、`skills` / `rules` glob |
| `hooks` | Hook 文件 glob |
| `safe_command_patterns` | Bash 自动放行的正则（白名单外的命令默认拦截） |
| `yolo` | 完全跳过危险命令审查 |
| `steer` | steer 模式开关 |
| `tool_result_truncate` | 超长工具结果截断（`max_length` / `keep_chars`，头尾保留 + 全文落临时文件） |
| `images` | 图片读入与请求期保留预算（`max_bytes` 4718592（4.5 MiB）/ `max_images` 32 / `count_quantum` 8 / `request_budget_bytes` 37748736 / `evict_quantum_bytes` 18874368，全部 > 0）——读图链路与投影算法见 [media-images.md](media-images.md) |
| `log.level` | 网关**控制台**级别（守护进程 stdout/stderr，被 `gateway.log` 捕获）；每日文件日志恒为 DEBUG |
| `gateway` | `host` / `port` / `remote_tool_timeout` / `auth`（opt-in API key：`enabled` + `keys[{key, role}]`，角色 `admin` / `tool_runtime`） |
| `commands.paths` | prompt 命令（`/xxx` 展开）的 glob 列表，每个 .md（frontmatter: name / description / aliases，正文 `$ARGUMENTS` 占位）定义一个命令 |
| `user_agent.preset` | HTTP User-Agent 预设（`opencode` / `qwen-code`） |
| `sessions` | 会话存储路径解析（env 覆盖优先）+ `eviction`（空闲会话逐出：`enabled` / `idle_ttl_seconds`（默认 1800）/ `sweep_interval_seconds`（默认 300，启动时读取）） |

## TUI 配置（`~/.wing/tui/config.yaml`）

`colors`（`preset` = `wing` | `terminal` 选底色：`wing` 是给暗底终端设计的一套 hex 灰阶 + cyan accent（默认），`terminal` 跟随终端自己的 ANSI 色；其余每个槽位键**覆盖该槽**：`accent` / `text` / `thinking` / `tool_result` / `dim` / `success` / `warning` / `danger` / `math` / `surface` 与 diff 行背景 tint（`diff_add_bg` / `diff_del_bg` 及词级强调 `diff_*_bg_strong`），命名色或 24-bit hex 都行，非法值 warn 后回落该预设槽位）、`layout`（输入区 / 弹窗 / 工具输出的行数上限）、`rendering`、`api_key`（网关鉴权，空则不发送）。调色 / 对色用 `cargo run -p wing --example theme_preview`（整条 transcript 的真 cell 画廊，`--preset` 切预设、`--html` 导出）。

`rendering` 的键：

| 键 | 取值 | 默认 | 说明 |
|----|------|------|------|
| `thinking` | `visible` \| `hidden` | `visible` | reasoning 块的**默认呈现**：`visible` = 展开渲染正文；`hidden` = 折叠成一行摘要（`⦁ 深度思考中 4s`，持续刷光、完成后定格时长）。`Ctrl+O` 全局切换详细 / 简略（所有轮一起、会话内保持），语义见 [`tui-rendering.md`](tui-rendering.md) 第二节·六 |
| `math` | `text` \| `off` | `text` | `text` = `$…$` / `$$…$$` / 裸 AMS 环境渲染成字符网格（渲染不了时显示完整 LaTeX 源码）；`off` = 完全不解析、不归一化，即未引入公式渲染前的行为。非法值 warn 后回落 `text` |
| `images` | `off` / `auto`（大小写不敏感） | `auto` | markdown 本地图片：`auto` = 启动时探测终端图形协议（kitty/sixel/iTerm2），支持就画真图；`off` 或探测失败 = 今天的链接路径（不探测、不读盘、零开销） |

非法的 `rendering.images` 值回退到 `auto` 并在 TUI 日志里 warn 一行；路径策略（workspace 相对 / 越界 / 远程 URL 一律退回链接路径）见 [`tui-rendering.md`](tui-rendering.md) 第五节，能力阶梯 / 资源上限 / 失效触发点 / **文件重写的新鲜度检查（1 s 窗口）** / 性能数字见 [`tui-images.md`](tui-images.md)。`math` / `thinking` / `images` 三个键都**大小写不敏感**：`wing tui --dump-config` 写出的 `Text` / `Off` / `Hidden`（`Serialize` 的变体名）读得回来，dump → 改 → 回填不会把开关悄悄改回默认。diff 的上下文行数不是前端配置——窗口由后端随载荷下发（见 `diff-payload-window`），前端按给定内容逐行渲染。

> `gateway.host/port` 只影响独立启动 `wing-gateway` 的场景；Rust TUI 读的是 backend config，不会读 TUI config 里的网关地址。

## 日志策略（前后端统一）

- 双方都按**本地日期**一天一个文件：`wing_YYYY-MM-DD.log`，append 模式打开——网关 / TUI 重启绝不截断或分裂日志；启动与每次轮转时 prune 7 天前的文件（含遗留命名）。
- 后端日志在 `~/.wing/core/logs/`（`new.log` 始终指向活跃后端日志）；TUI 日志在 `~/.wing/tui/logs/`（命名相同，**无**符号链接）。
- 日志初始化是显式的：网关 CLI（`wing-gateway` → `wing.common.logger.setup_logger`）与 `wing` 二进制（`cmd::dispatch` 入口统一初始化，TUI / stdio / 全部编排子命令共用同一份 `util/logging.rs`，幂等）在启动时挂 handler。**import `wing` 没有任何日志副作用**——测试与脚本永远不会在 `~/.wing` 创建文件。
- 网关 lifespan 里把 asyncio 未处理异常的兜底接进 wing 日志：`asyncio unhandled: message=… task=…`（带 traceback），随后原样转发给原处理器（stderr 行为不变）——「Task exception was never retrieved / Task was destroyed but it is pending」这类暗角不再只沉在 `gateway.log`（`common/logger.py::install_loop_exception_logger`）。
- 后端每行格式 `YYYY-MM-DD HH:MM:SS - LEVEL - path:line - message`；TUI 由 tracing 输出（本地时间，`RUST_LOG` 可覆盖级别，默认 `wing=warn`）。
- 前后端一律使用本地时间，时间范围 grep 可直接工作：

```bash
grep '^2026-09-08 23:' ~/.wing/core/logs/new.log
awk '$0 >= "2026-09-08 23:10" && $0 < "2026-09-08 23:30"' ~/.wing/tui/logs/wing_2026-09-08.log
```

## 环境变量

| 变量 | 作用 |
|------|------|
| `WING_HOME` | 覆盖 `~/.wing`（后端数据在 `$WING_HOME/core`，TUI 在 `$WING_HOME/tui`） |
| `WING_SESSIONS_PATH` | 覆盖 sessions 目录 |
| `RUST_LOG` | TUI tracing 级别（默认 `wing=warn,tokio_tungstenite=warn`） |
