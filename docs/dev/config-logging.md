# 配置与日志

## 目录布局（WING_HOME）

`WING_HOME` 覆盖 `~/.wing`；后端数据统一落在 `$WING_HOME/core`（`wing/config.py::get_wing_home()`）。`WING_SESSIONS_PATH` 额外覆盖 sessions 目录。

```
~/.wing/
├── core/
│   ├── config.yaml     后端配置（唯一事实来源；缺失时由 default_config.py 模板生成）
│   ├── logs/           后端日志
│   │   ├── wing_YYYY-MM-DD.log            网关运行时日志（按本地日期，append；恒 DEBUG）
│   │   ├── new.log → wing_YYYY-MM-DD.log  指向活跃后端日志的符号链接（仅后端；每次轮转 / setup 刷新）
│   │   └── gateway.log                    网关守护进程 stdout/stderr（uvicorn 错误、traceback；append）
│   ├── sessions/       会话持久化（metadata + history.jsonl + aux / metrics.json）
│   ├── metrics.json    全局指标（LLM / 工具调用 / 压缩，按天聚合）
│   └── metrics_experimental.json  BetterEdit 实验审计
└── tui/
    ├── config.yaml     TUI 配置（colors / layout / rendering / goal / api_key）
    └── logs/           TUI 日志：wing_YYYY-MM-DD.log（命名同后端，无符号链接）
```

## 后端 config.yaml

顶层键（手写注释模板即 `wing/default_config.py`，改 Config 字段时须同步维护）：

| 键 | 说明 |
|----|------|
| `providers` | LLM provider 列表（**必填**）。每项声明 `protocol: openai \| anthropic`、`base_url`、`api_key`，以及超时（`timeout_first_chunk` / `timeout_total`）、重试（`max_retries` / `max_retry_delay`）、`explicit_cache_mode`、`reasoning_effort`、`extra_body` 等；Anthropic 需 `max_tokens` / `anthropic_version`，可配 `models` 静态列表跳过远端查询 |
| `agents` | Agent 模板列表（**必填**）：`model`（+ `provider` 引用）、`default`、`system_prompt`、`tools`、`context_window_tokens` / `keep_recent_tokens`、`skills` / `rules` glob |
| `hooks` | Hook 文件 glob |
| `safe_command_patterns` | Bash 自动放行的正则（白名单外的命令默认拦截） |
| `yolo` | 完全跳过危险命令审查 |
| `steer` | steer 模式开关 |
| `tool_result_truncate` | 超长工具结果截断（`max_length` / `keep_chars`，头尾保留 + 全文落临时文件） |
| `log.level` | 网关**控制台**级别（守护进程 stdout/stderr，被 `gateway.log` 捕获）；每日文件日志恒为 DEBUG |
| `gateway` | `host` / `port` / `remote_tool_timeout` / `auth`（opt-in API key：`enabled` + `keys[{key, role}]`，角色 `admin` / `tool_runtime`） |
| `commands.paths` | prompt 命令（`/xxx` 展开）的 glob 列表，每个 .md（frontmatter: name / description / aliases，正文 `$ARGUMENTS` 占位）定义一个命令 |
| `user_agent.preset` | HTTP User-Agent 预设（`opencode` / `qwen-code`） |
| `sessions` | 会话存储路径解析（env 覆盖优先） |

## TUI 配置（`~/.wing/tui/config.yaml`）

`colors`、`layout`（输入区 / 弹窗 / diff / 工具输出的行数上限）、`rendering`、`goal.checker_system_prompt`、`api_key`（网关鉴权，空则不发送）。

> `gateway.host/port` 只影响独立启动 `wing-gateway` 的场景；Rust TUI 读的是 backend config，不会读 TUI config 里的网关地址。

## 日志策略（前后端统一）

- 双方都按**本地日期**一天一个文件：`wing_YYYY-MM-DD.log`，append 模式打开——网关 / TUI 重启绝不截断或分裂日志；启动与每次轮转时 prune 7 天前的文件（含遗留命名）。
- 后端日志在 `~/.wing/core/logs/`（`new.log` 始终指向活跃后端日志）；TUI 日志在 `~/.wing/tui/logs/`（命名相同，**无**符号链接）。
- 日志初始化是显式的：网关 CLI（`wing-gateway` → `wing.common.logger.setup_logger`）与 TUI（`util/logging.rs`）在启动时挂 handler。**import `wing` 没有任何日志副作用**——测试与脚本永远不会在 `~/.wing` 创建文件。
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
