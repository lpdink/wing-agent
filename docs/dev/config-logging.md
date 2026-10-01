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
│   ├── sessions/       会话持久化（metadata + history.jsonl + aux / metrics.json）；.media/ 为跨会话共享的图片媒体池（内容寻址）
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
| `providers` | LLM provider 列表（**必填**）。每项声明 `protocol: openai \| anthropic`、`base_url`、`api_key`，以及超时（`timeout_first_chunk` = **响应头**超时、`timeout_total` = 总时长；另有硬编码 120s 的**响应体停滞**判定，见 `provider/transport.py` 的 `STREAM_IDLE_TIMEOUT`——响应头到达后两次读取间隔超过它即判停滞并走 `with_retry`）、重试（`max_retries` / `max_retry_delay`）、`explicit_cache_mode`、`reasoning_effort`、`extra_body` 等；Anthropic 需 `max_tokens` / `anthropic_version`。`models` 静态列表跳过远端查询，元素为字符串（存量）或对象（`name` = 实际调用名 / `display_name` / `description` / `capabilities: {vision: true}`；未声明 = text-only，键名拼错静默忽略）。图片相关：`image_delivery`（`inline \| followup`，缺省按协议：openai → followup、anthropic → inline）、`image_max_bytes`（单图请求期兜底，见 [media-images.md](media-images.md)） |
| `agents` | Agent 模板列表（**必填**）：`model`（+ `provider` 引用）、`default`、`system_prompt`、`tools`、`context_window_tokens` / `keep_recent_tokens`、`skills` / `rules` glob |
| `hooks` | Hook 文件 glob |
| `safe_command_patterns` | Bash 自动放行的正则（白名单外的命令默认拦截） |
| `yolo` | 完全跳过危险命令审查 |
| `steer` | steer 模式开关 |
| `tool_result_truncate` | 超长工具结果截断（`max_length` / `keep_chars`，头尾保留 + 全文落临时文件） |
| `images` | 图片读入与请求期保留预算（`max_bytes` 8388608 / `max_images` 32 / `count_quantum` 8 / `request_budget_bytes` 37748736 / `evict_quantum_bytes` 18874368，全部 > 0）——读图链路与投影算法见 [media-images.md](media-images.md) |
| `log.level` | 网关**控制台**级别（守护进程 stdout/stderr，被 `gateway.log` 捕获）；每日文件日志恒为 DEBUG |
| `gateway` | `host` / `port` / `remote_tool_timeout` / `auth`（opt-in API key：`enabled` + `keys[{key, role}]`，角色 `admin` / `tool_runtime`） |
| `commands.paths` | prompt 命令（`/xxx` 展开）的 glob 列表，每个 .md（frontmatter: name / description / aliases，正文 `$ARGUMENTS` 占位）定义一个命令 |
| `user_agent.preset` | HTTP User-Agent 预设（`opencode` / `qwen-code`） |
| `sessions` | 会话存储路径解析（env 覆盖优先）+ `eviction`（空闲会话逐出：`enabled` / `idle_ttl_seconds`（默认 1800）/ `sweep_interval_seconds`（默认 300，启动时读取）） |

## TUI 配置（`~/.wing/tui/config.yaml`）

`colors`（含 diff 行背景 tint：`diff_add_bg` / `diff_del_bg` 与词级强调 `diff_*_bg_strong`，24-bit hex；`math` = 公式颜色，默认 `cyan`）、`layout`（输入区 / 弹窗 / 工具输出的行数上限）、`rendering`、`goal.checker_system_prompt`、`api_key`（网关鉴权，空则不发送）。

`rendering` 的键：

| 键 | 取值 | 默认 | 说明 |
|----|------|------|------|
| `thinking` | `visible` \| `hidden` | `visible` | reasoning 块：完整渲染 / 只显示事件计数 |
| `math` | `text` \| `off` | `text` | `text` = `$…$` / `$$…$$` / 裸 AMS 环境渲染成字符网格（渲染不了时显示完整 LaTeX 源码）；`off` = 完全不解析、不归一化，即未引入公式渲染前的行为。非法值 warn 后回落 `text` |
| `images` | `off` / `auto`（大小写不敏感） | `auto` | markdown 本地图片：`auto` = 启动时探测终端图形协议（kitty/sixel/iTerm2），支持就画真图；`off` 或探测失败 = 今天的链接路径（不探测、不读盘、零开销） |

非法的 `rendering.images` 值回退到 `auto` 并在 TUI 日志里 warn 一行；路径策略（workspace 相对 / 越界 / 远程 URL 一律退回链接路径）见 [`tui-rendering.md`](tui-rendering.md) 第五节，能力阶梯 / 资源上限 / 失效触发点 / **文件重写的新鲜度检查（1 s 窗口）** / 性能数字见 [`tui-images.md`](tui-images.md)。`math` / `thinking` / `images` 三个键都**大小写不敏感**：`wing tui --dump-config` 写出的 `Text` / `Off` / `Hidden`（`Serialize` 的变体名）读得回来，dump → 改 → 回填不会把开关悄悄改回默认。diff 的上下文行数不是前端配置——窗口由后端随载荷下发（见 `diff-payload-window`），前端按给定内容逐行渲染。

> `gateway.host/port` 只影响独立启动 `wing-gateway` 的场景；Rust TUI 读的是 backend config，不会读 TUI config 里的网关地址。

## 日志策略（前后端统一）

- 双方都按**本地日期**一天一个文件：`wing_YYYY-MM-DD.log`，append 模式打开——网关 / TUI 重启绝不截断或分裂日志；启动与每次轮转时 prune 7 天前的文件（含遗留命名）。
- 后端日志在 `~/.wing/core/logs/`（`new.log` 始终指向活跃后端日志）；TUI 日志在 `~/.wing/tui/logs/`（命名相同，**无**符号链接）。
- 日志初始化是显式的：网关 CLI（`wing-gateway` → `wing.common.logger.setup_logger`）与 `wing` 二进制（`cmd::dispatch` 入口统一初始化，TUI / stdio / 全部编排子命令共用同一份 `util/logging.rs`，幂等）在启动时挂 handler。**import `wing` 没有任何日志副作用**——测试与脚本永远不会在 `~/.wing` 创建文件。
- 后端每行格式 `YYYY-MM-DD HH:MM:SS - LEVEL - path:line - message`；TUI 由 tracing 输出（本地时间，`RUST_LOG` 可覆盖级别，默认 `wing=warn`）。
- 前后端一律使用本地时间，时间范围 grep 可直接工作：

```bash
grep '^2026-09-08 23:' ~/.wing/core/logs/new.log
awk '$0 >= "2026-09-08 23:10" && $0 < "2026-09-08 23:30"' ~/.wing/tui/logs/wing_2026-09-08.log
```

## interrupt 取证日志（runbook）

`POST /api/session/interrupt` 全程留下分段日志（网关端点 → agent → 锁 → cancel 看门狗），
用于定位「interrupt 请求永不返回」这类现场——会话本身可能毫发无损，而锁死不释放
（`interrupt()` 里 `await old` 无超时、cancel 只调一次：一次未生效的 cancel 就足以
让后续所有 interrupt 排队）。实现与栈链覆盖范围见 `wing/agent/cancel_watch.py` 模块文档。

一次正常 interrupt 的日志链（`request_id` 由端点生成、透传到 agent，两侧按同一 id 关联；
agent 侧 tag 取前 8 位）：

```
interrupt request: session_id=… client=127.0.0.1:63525 client_id=- request_id=58a40cb0…
interrupt start [<sid> req=58a40cb0]: hooks=1 lock_waiters=0 worker=Task-11(0x…)
interrupt hooks [<sid>]: firing 1 hook(s): Bash pid=4242
interrupt lock acquired [<sid> req=58a40cb0]: waited 0ms (queued=0)
cancel snapshot [<sid> req=58a40cb0]: task=Task-11(0x…) done=False cancelling=0 must_cancel=False fut_waiter=Future(pending) <Future pending …> stack=[queues.py:158:get <- inbox.py:43:get <- react_loop.py:141:run_turn <- core.py:517:_run]
interrupt old worker retired [<sid> req=58a40cb0]: await_result=cancelled waited=0ms done=True cancelled=True cancelling=1
Agent interrupted and reset [<sid> req=58a40cb0] total=1ms
interrupt done: session_id=… request_id=58a40cb0… elapsed_ms=1
```

排查顺序（目标：日志本身即可定位，无需 lldb 注入）：

1. `grep 'interrupt lock' new.log` —— 出现 `still NOT acquired` / `held … holder stuck?`
   即锁已死：告警自带持锁时长、持锁者 tag 与 waiter 数（间隔翻倍退避，封顶 60s）。
2. 看 `cancel snapshot` —— cancel 那一刻 worker 挂在哪。`fut_waiter` 的**类型**是判别器：
   `Future`（Queue / httpx 读 / sleep）、`_GatheringFuture`（工具 gather）、`Task`（在等
   另一个 task）指向完全不同的吞没路径；`stack=[…]` 沿协程等待链下钻（协程 / async
   generator / Task / 单 child 的 gather，`<gather ×N>` 标记多 child 分叉），覆盖到
   httpx/httpcore 一类第三方栈帧；`await Future` 的链末端没有帧，由 `fut_waiter=` 承担。
3. 看 `cancel watchdog […]`（T+1/5/15s 复查未死时的全量 dump）与
   `worker loop boundary … cancelling 0 -> 1` —— 两次 dump 栈不同 = cancel 已投递但被
   某帧吞掉后继续跑；纹丝不动 = 从未投递。**看门狗完全没有输出**是第三种签名：日志停在
   `interrupt start` 之后——看门狗与被观测者同 loop，指向"循环被同步调用卡住"一类问题。
4. `interrupt hooks … Bash pid=…` 对齐 history 里的 `[exit code: -9]` —— 归因「排队中的
   interrupt 仍会先杀一次前台工具」（hook 副作用在拿锁之前执行）。
5. `grep 'with_retry\|模型生成' new.log` —— 重试日志的异常类型分布：无 `*Timeout` 类重试
   可排除 anyio CancelScope 吞外来 cancel 的路径；也别忘了 `asyncio unhandled: …`
   （loop 异常处理器兜底，含 traceback）。

## 环境变量

| 变量 | 作用 |
|------|------|
| `WING_HOME` | 覆盖 `~/.wing`（后端数据在 `$WING_HOME/core`，TUI 在 `$WING_HOME/tui`） |
| `WING_SESSIONS_PATH` | 覆盖 sessions 目录 |
| `RUST_LOG` | TUI tracing 级别（默认 `wing=warn,tokio_tungstenite=warn`） |
