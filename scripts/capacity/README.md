# scripts/capacity — 容量测量（阶梯扫描）

回答一个 `scripts/perf/` 与 `libs/wing-probe/` 都回答不了的问题：
**网关能同时扛住多少个并发活跃会话，从哪里开始退化，退化在内存还是速度上。**

做法：串行逐档扫描 N 个并发会话，每档起一套隔离环境（fresh `WING_HOME` + 真网关子进程 +
进程内假 Provider + K 个 driver 分片子进程），预热后在一个固定长度的采样窗口里采
网关侧 CPU / RSS / fd、投递延迟、轮次耗时与错误数，产出机器可读的 `run.json` +
人读 `report.md` + 每档原始样本。

这是**手工测量工具**：读数是机器相关的，因此不进 `make test`、不做 CI 门禁。

## 常用命令

```bash
# 离线自测（≤60 s、不联网、不需要 API key；改这个工具后先跑它）
make capacity-selftest                      # = uv run python scripts/capacity/run.py --selftest

# 小档位验证（先确认读数合理，再上大档）
uv run python scripts/capacity/run.py --levels 1,10 --dwell 5 --warmup 2

# 主力扫描（单机串行，跑之前把 fd 上限抬高；见「fd 上限」一节）
ulimit -n 8192 && uv run python scripts/capacity/run.py \
    --levels 1,10,50,100,250,500,1000 --rate 50 --dwell 20

# 同一件事经 make（旗标统一走 CAPACITY_ARGS，默认值只有 run.py 一处）
make capacity CAPACITY_ARGS="--levels 1,10,50 --dwell 20 --json"
```

CLI（冻结；下游脚本依赖这些名字）：

| 参数 | 默认 | 含义 |
|---|---|---|
| `--scenario` | `pure_stream` | 负载形态（见「负载形态」） |
| `--levels` | `1,10,50,100` | 逗号分隔的会话数阶梯（严格升序） |
| `--rate` | `50` | **每会话** token/s（帧大小 1 token、帧间隔 `1/rate` 秒） |
| `--dwell` | `20` | 每档采样窗口（秒） |
| `--warmup` | `5` | 每档预热（秒，丢弃） |
| `--shards` | `4` | driver 分片子进程数（钳到 `min(shards, sessions)`） |
| `--subscribers` | `per-session` | `per-session` = 每会话一条 WS（贴近真实）；`per-shard` = 每分片一条（轻量档） |
| `--history-tokens` | `0` | 预置历史体量；只接受 `0`（其余形态待补） |
| `--out` | `target/capacity/<UTC 时间戳>` | 产物目录 |
| `--selftest` | — | 离线自测 |
| `--json` | — | stdout 只输出一行 JSON 摘要（`ok` / `out` / `run_json` / `report` / `levels[]` / `failures`）；`levels[]` 每项是 `sessions` + `gateway_cpu_cores` / `delivery_p99_ms` / `tokens_per_second_actual` / `errors` / `error` 的投影，人读路径用同一份数据 |

退出码：`0` 全部档位完成；`2` 参数 / 环境错误（未启动任何进程）；`3` 有档位失败
（仍落盘产物，供取证）。`EMFILE` 之类「跑到一半才炸」的失败不会产出漂亮空表：
fd 上限在起任何子进程之前就抬好，抬不动直接退出（见「fd 上限」）。

## 进程拓扑（为什么这样分层，以及它如何避免测出假的退化）

```
coordinator 进程（run.py）
├── 假 Provider：同进程内 aiohttp（复用 wing_probe.provider.FakeProvider）
│     └── 逐帧发射时刻在内存里（配对投递延迟用）
├── 网关：独立子进程（wing_probe.env.ProbeEnv → fresh WING_HOME + OS 分配端口）
└── driver 分片 ×K：独立子进程（driver_shard.py，只经公开 WS / HTTP 驱动）
```

三种角色都会把一个核烧掉，放错位置就会把「客户端饱和」记成「网关退化」：

| 角色 | 谁会烧核 | 放错位置的后果 |
|---|---|---|
| 假 Provider | 每档 `N × rate` 帧/秒全部由它产出 | 与 driver 同进程 → 两者抢同一个事件循环，读不出是谁在饱和 |
| driver 分片 | 与收到的帧数同阶 | 同上；且「产出端慢」与「消费端慢」无法分辨 |
| 网关（被测对象） | 被测变量 | 与任何一方同进程 → CPU / RSS 采样量到的是混在一起的进程 |

所以：**网关必须是独立进程**（否则没有可采样的 pid）；**Provider 与 driver 必须分开**
（两者 CPU 同阶）；**driver 必须分片**（单进程消费 `N × rate` 帧会先在自己的 GIL 上饱和）。

Provider 留在 coordinator 进程内，是因为它本来就设计成同进程对象（剧本注册与请求
留档都是同步读），而 coordinator 在采样窗口内除了读几次进程统计以外不做事——它的
CPU 单独上报为 `rig.provider_cpu_cores`（Provider 宿主 + 采样器，是上界）。

判读读数的第一件事：看 `rig.provider_cpu_cores + Σ rig.shard_cpu_cores` 离核数有多远。
接近核数 ⇒ 这一档测的是 rig，不是网关，读数标注为不可信。

三个 CPU 数字是**同一个窗口口径**（窗口起止的累计 CPU 差 / 窗口秒）：网关由 coordinator
外部采样，Provider 与分片在各自进程里自记账（起点钉在 `window_start`，收到窗口消息时
并不起算——那时还在 warmup）。采网关的 RSS / fd 要走 `ps` / `lsof` 子进程（合计约 17 ms），
所以放在线程里跑：同步跑会每秒卡住一次正在产出负载的 Provider，把抖动记进读数。

## 产物

```
<out>/
├── run.json                       # 机器可读（字段名冻结）
├── report.md                      # 人读表格 + 口径脚注
└── raw/<N>/                       # 每档原始样本
    ├── shard-<k>.samples.json     # 该分片的每次轮次 + 每帧到达时刻（相对窗口的微秒）
    ├── shard-<k>.json             # 分片配置（会话分配 / 订阅模式）
    ├── shard-<k>.log              # 分片 stderr（崩溃取证）
    └── gateway.log                # 该档网关日志（崩溃 / 断连取证用）
```

`run.json` 的冻结字段（**取不到就是 `null`，绝不写 0**）：

```jsonc
{
  "meta": {
    "tool": "capacity", "schema": 1, "scenario": "pure_stream",
    "started_at": "<ISO8601Z>", "duration_s": 123.4,
    "args": {"levels": [1,10], "rate": 50.0, "dwell": 20.0, "warmup": 5.0,
             "shards": 4, "subscribers": "per-session", "history_tokens": 0},
    "host": {"cpus": 12, "mem_gb": 16.0, "platform": "darwin", "ulimit_n": 4256},
    "gateway": {"pid": 123, "version": "0.7.0", "commit": "a7e7126"},
    "session_create": {"ok": 11, "failed": 0, "total_ms": 25.5, "scope": "sum over levels"},
    "notes": ["…"], "failures": [], "out": "<out dir>"
  },
  "levels": [{
    "sessions": 10, "wall_s": 5.08,
    "metrics": {
      "gateway.cpu_cores": 0.12, "gateway.rss_mb_peak": 73.2,
      "gateway.rss_mb_end": 73.2, "gateway.rss_mb_slope_per_session": 0.3,
      "gateway.fds_peak": 47,
      "delivery.p50_ms": null, "delivery.p99_ms": null, "delivery.p999_ms": null,
      "turn.p50_ms": 1680.5, "turn.p99_ms": 1741.5,
      "tokens_per_second_actual": 395.5, "errors": 0, "slow_client_recycles": 0
    },
    "rig": {"provider_cpu_cores": 0.029, "shard_cpu_cores": [0.018, 0.015, 0.012, 0.013],
            "host_loadavg": [4.32, 5.26, 4.74]},
    "samples": {"count": null, "dropped": null, "dropped_reasons": null,
                "text_frames": 2010, "turns": 20, "delivery_available": false},
    "session_create": {"ok": 10, "failed": 0, "total_ms": 13.9},
    "gateway": {"pid": 456, "url": "http://127.0.0.1:63665",
                "version": "0.7.0", "commit": "a7e7126"},
    "shape": {"rate": 50.0, "shards": 4, "subscribers": "per-session"},
    "notes": ["…"]
  }]
}
```

**成功档与失败档的键集完全相同**（同一个 `level_skeleton()`）：失败档只是多一个 `error`
字符串、值全为 `null`——下游按冻结字段名直取不会 `KeyError`。

追加字段（不改上面任何冻结名，读方可以忽略）：

- `meta.notes` / `meta.failures` / `meta.out` / `meta.selftest`：全局说明、失败摘要、
  产物目录、自测标记；
- `meta.session_create` 是**全档汇总**（`scope: "sum over levels"`）——逐档数字在
  `levels[i].session_create`（「建 N 个会话要多久」本身就是被测数字）；有档位没拿到
  数字（环境都没起来）时整组写 `null` + `scope` 说明，不按 0 求和；
- `levels[i].gateway` 是本档网关身份（每档都是新进程，`meta.gateway` 放最后一档）；
- `levels[i].samples` 给 `text_frames`（窗口内收到的帧数）、`turns`（窗口内完成的轮次数）、
  `delivery_available`、`dropped_reasons`（配对丢弃的分类计数）；
- `levels[i].metrics.gateway.rss_mb_end`：窗口末次 RSS（`rss_mb_slope_per_session` 与报告
  的 `peak / end` 列都用它）；
- `levels[i].rig.host_loadavg`：**窗口起点**采一次（三档数值不同说明机器状态在变）；
- `levels[i].shape`：本档的 rate / 实际分片数 / 订阅模式；
- `levels[i].error`：该档失败时的诊断（此时该档没测到的指标为 `null`）。

## 口径

| 指标 | 定义 |
|---|---|
| `gateway.cpu_cores` | 窗口内网关进程 CPU 秒 / 窗口秒（首尾两次**累计值**之差；不做周期采样，否则采样器自己的开销会混进来） |
| `gateway.rss_mb_peak` / `rss_mb_end` | 窗口内每 `1 s` 采样一次的峰值 / 末次值（Linux `/proc/<pid>/statm`，macOS `ps -o rss=`） |
| `gateway.fds_peak` | 同上频率采样的 fd 数峰值（Linux `/proc/<pid>/fd`，macOS `lsof -p`）——峰值分辨率 = 采样间隔 |
| `gateway.rss_mb_slope_per_session` | 与**上一档**末次 RSS 的相邻差斜率；最小档无上一档 → `null` |
| `delivery.*_ms` | 客户端收到第 i 帧 `text` 的时刻 − 假 Provider 发射第 i 帧的时刻（同机 `time.monotonic()`） |
| `turn.*_ms` | 发 prompt → 收到 `turn_result`（只取窗口内发起**且**窗口内完成的轮次）。`error` 事件不是轮次边界——它是失败轮的伴随事件 |
| `tokens_per_second_actual` | 窗口内收到的 `text` 事件数 / 窗口秒（`pure_stream` 里 1 帧 = 1 token） |
| `errors` | 窗口内（事件到达时刻在 `[window_start, window_end]` 内）的 `turn_result.is_error` 数 + 分片发送失败数。失败轮上网关会**同时**发 `turn_result(is_error)` 与 `error` 事件，只记前者的原因见 :mod:`driver_shard` |
| `slow_client_recycles` | 窗口结束前就被网关回收的客户端连接数（分片在 `window_end` 时刻数一遍自己的连接：此刻还没关连接，`closed` 的都是网关先断的）。不扫网关日志——那会把窗口末驱动自己的正常收尾也算进去 |
| `rig.provider_cpu_cores` | coordinator 进程（= Provider 宿主 + 采样器）在窗口内的 CPU 秒 / 窗口秒；上界，不是纯 Provider |
| `rig.shard_cpu_cores[]` | 每个分片**自报**的窗口内 CPU 秒 / 分片自己的记账区间（起点钉在 `window_start`，因此与上面两个同口径、可相加）；分片上报的区间与采样窗口差 >10% 时留一条 note |

百分位是 **nearest-rank**（p99 = 升序第 `ceil(0.99·n)` 个值，不插值），与
`scripts/perf/suite_gateway.py` 一致；实现只在 `metrics.percentile()` 一处。

### 投递延迟的配对（严格，不猜）

1. driver 每次发的 prompt 文本全局唯一（`load:<session>:<seq>`）；
2. coordinator 在假 Provider 的 `RequestLog` 里按「**最后一条 user 消息正文** == 该文本」
   定位 `request_index`（`content` 可能是 block 列表，取其中的 `text` 块拼接）；
3. 用该请求的发射时刻列表与客户端按序到达的 `text` 事件**按下标配对**：
   - 长度不等 → 整轮丢弃（不截断、不偏移对齐）；
   - 同一 prompt 命中多条请求（网关重试）→ 整轮丢弃（无法确定是哪一次）；
   - 负延迟 → 视为错位，丢弃该帧。

丢弃的帧按原因计数进 `levels[i].samples.dropped_reasons`（**单位是帧**，
`sum(reasons) == dropped`），并写进 `notes`。`delivery_available` 在失败档是 `null`
（未知），不是 `false`。

## 负载形态

当前只有 `pure_stream`（`shape_pure_stream.py`）：每会话一条持续的长流式回复，帧大小
1 token、帧间隔 `1/rate`，一轮结束立刻发下一条 prompt。它把「网关的每帧成本 × 会话数」
直接顶出来，是探并发边界的主负载。单轮时长由档位驱动时长反推（默认目标 5 s，窗口更短
时按 `span/5` 缩短），保证窗口内能完成的轮次数量级稳定。

新增形态：写一个满足 `NAME` / `MODEL` / `script(*, sessions, rate, span_s)` /
`prompt_text(session_id, seq)` 契约的模块，在 `shapes.SHAPES` 登记一行
（`shapes.resolve()` 会校验契约，跑 `--selftest` 的 `shapes.contract` 一项即可）。

## fd 上限

每个并发会话大致吃 4 个 fd（一条 WS + 一条上游 HTTP 连接，再留一倍余量）。默认软限
（本机开发 shell 常见 256）在几十个会话上就会 `EMFILE`，而 `EMFILE` 出现在窗口中途时
产出的是一张「好看的空表」——所以：

1. `run.py` 在**起任何子进程之前**把 `RLIMIT_NOFILE` 软限抬到
   `max(当前软限, 4 × N_max + 256)`（封顶硬限），子进程继承；
2. 实际生效值写进 `meta.host.ulimit_n`；
3. 抬不到目标就带可行动的错误退出（附当前 / 目标 / 硬限），而不是跑一半才炸。

`make capacity` 目标另有一层 `ulimit -n 8192`（`CAPACITY_NOFILE` 可覆盖）。

## 与 scripts/perf、wing-probe 的关系

- `scripts/perf/` 用同一套「真网关 + 假 Provider」驱动**一个**会话做 A/B 回归；
  容量工具驱动 N 个会话做**绝对值**扫描。两者都不改产品、都不碰 CI 门禁。
- 进程采样（CPU 的 darwin / linux 分支）与百分位秩定义**照搬** perf 的实现与口径，
  但各有各的一份：那边住在一个套件脚本里（不是共用模块），跨工具 import 会把整条
  perf 测量链拖进来。**origin 是 perf 那一份**（更早、被 CI 覆盖）——改任何一边都要同步
  另一边，本目录的实现是派生。第三处同款是原子写 JSON（`run.write_json` vs
  `perf/common.write_json`，十行）；把它抽成共用模块需要动 `scripts/perf/**`，
  那不在本工具的改动范围内。
- `libs/wing-probe/` 是本工具的底座（`ProbeEnv` 起环境、`FakeProvider` 当上游、
  `GatewayWS` 当传输层参考实现）。分片为什么不用 probe 的 `Driver` / `Session`：

  1. `Driver` 用**一条** WS 承载它挂载的全部会话（会话靠事件里的 `session_id` 路由），
     撑不起 `--subscribers per-session`（每会话一条 WS）这个默认拓扑；
  2. `Session.chat()` 是「发一条 → 等整轮」的阻塞调用，没有「一轮结束立刻发下一条、
     直到窗口结束」的泵；
  3. 会话由 coordinator 统一建（建会话耗时是被测数字），与 `Driver.session()` 的职责冲突。

  也不用 probe 的 `Timeline`：它永久保留每个事件，1000 会话 × 50 帧/秒 会让分片先 OOM
  （分片的原始帧日志有界保留 64 条，只作诊断）。

## 自测（`--selftest`）

≤60 秒（当前约 9 s）、不联网、不需要 API key、可重复；`make check-python` 会跑它：

| 检查 | 证明什么 |
|---|---|
| `stats.percentile` | 百分位秩定义的 known answer（含 n=1 / 偶数 / 极值 / 空序列报错） |
| `pairing.last_user_text` | 两种 body 形态（str / block 列表）都能取到配对键 |
| `pairing.delivery` | 配对数学（延迟 = 到达 − 发射）+ 四类丢弃原因 + 缺接口抛错 |
| `pairing.assemble` | 配对结果落到 `run.json` 的接线（延迟可用这一支、全丢弃这一支、窗口不一致的 note） |
| `proc.sampling` | 自身 CPU 单调、RSS > 0、fd ≥ 3、fd 软限抬升到位 |
| `run.summary` | `--json` 一行摘要的字段名与取值投影（下游脚本的契约） |
| `cli.contract` | 冻结 CLI 的默认值与全部校验分支（非法 levels / rate / dwell / warmup / shards / history-tokens / scenario） |
| `shapes.contract` | 形态契约与剧本覆盖度、prompt 文本全局唯一 |
| `report.render` | 合成 `run.json` → markdown（表格列数自洽、缺数据是 `—` 不是 0、不可用有横幅、多行 note 折行） |
| `e2e` | **真网关子进程 + 真分片子进程**跑完迷你一档：断言 `run.json` / `report.md` / `raw/<N>/` 的形状、分片 CPU 窗口与采样窗口一致、健康档 `slow_client_recycles == 0`；再跑一次**相对 `--out`**（默认形态）；并用一个失败档断言「失败档键集 == 成功档键集」 |

`e2e` 走的是产品代码同一路径（注入 `run_ladder`），不是平行实现。

## 已知边界

- **投递延迟依赖上游接口**：`FakeProvider.frame_emits` / `emits_for`（逐帧发射时刻）。
  接口缺席时 `delivery.*` 与 `samples.count/dropped` 都是 `null`，`samples.delivery_available=false`，
  且 `meta.notes` / `levels[i].notes` / `report.md` 顶部三处点名——**不写 0、不估计**。
  其余指标（CPU / RSS / fd / 轮次 / 帧速率）照常可用。配对假设发射时刻列表与客户端
  `text` 事件一一对应（含首帧、不含 role / finish / usage 帧）；长度不等即判丢弃。
- `--history-tokens` 只接受 0（预置历史体量的形态待补）。
- 订阅模式不同，单会话成本不同：`per-session` 是 N 条 WS（贴近真实，开销更大），
  `per-shard` 是 K 条（轻量档，仅用于自测 / 极限档）。两者读数不可直接对比。
- 一档内所有会话共用一个 workspace 目录（工作目录不影响被测变量）；成功档收档时删掉，
  失败档保留（`raw/<N>/workspace`）以便复盘。
- 会话工具集固定为 `Read` 一个（`orchestrator.TOOLS`）：真实部署的工具 schema 与 system
  prompt 更大、会摊进每轮请求，因此这里量到的是**最小工具集下的上界**（会把容量报高）。
- 单轮之间有网关的提交 + 下一次请求组装开销，占空比因此低于 100%
  （`tokens_per_second_actual` 是真实值；小档位实测约 80~85%）。
- fd 峰值与 RSS 峰值的分辨率是 1 s 采样间隔，短尖峰可能采不到。
- 一档的全部分片样本会一次性读进 coordinator 内存再配对（1000 会话、每会话数轮、
  每轮数百帧的量级约几十 MB）——这是「配对要跨进程读两边」的代价，不是可省的开销。
- 分片崩溃 / 网关提前退出会让该档记 `error`、指标全 `null`，**其余档继续**跑
  （退出码 3）。现场（`raw/<N>/gateway.log` 与分片日志）保留。

## 排错：症状 → 先看哪里

| 症状 | 先看 |
|---|---|
| 某档指标全 `null` | `levels[i].error`；`raw/<N>/gateway.log`、`raw/<N>/shard-*.log` |
| `delivery.*` 全 `null` | `samples.delivery_available`、`meta.notes`（上游发射时刻是否就位） |
| `samples.dropped` 很大 | `samples.dropped_reasons`：`length_mismatch` = 帧序对不上；`ambiguous_prompt` = 网关重试 |
| 网关 CPU 高但 rig 也高 | 加大 `--shards`；看 `rig.shard_cpu_cores` 是否有个别分片吃满 |
| `EMFILE` / `Too many open files` | `meta.host.ulimit_n` 是否达到 `4 × N + 256`；见「fd 上限」 |
| 读数在两档之间抖动 | `rig.host_loadavg`（机器是否被别的任务占用）；容量测量必须串行跑 |
| 分片报 `FileNotFoundError: ... shard-0.json` | 产物目录不是绝对路径（`RunConfig` 会规范化；自己构造 `RunConfig` 时别绕过它） |
