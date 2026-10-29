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
# 离线自测（≤120 s、不联网、不需要 API key；改这个工具后先跑它）
make capacity-selftest                      # = uv run python scripts/capacity/run.py --selftest

# 小档位验证（先确认读数合理，再上大档）
uv run python scripts/capacity/run.py --levels 1,10 --dwell 5 --warmup 2

# 主力扫描：纯长流式（单机串行，跑之前把 fd 上限抬高；见「fd 上限」一节）
ulimit -n 8192 && uv run python scripts/capacity/run.py \
    --scenario pure_stream --levels 1,10,50,100,250,500,1000 \
    --rate 50 --chunk 1 --dwell 20 --warmup 5 --shards 4

# 每轮固定成本（近零事件的轮次：1 字符正文 + 一次 Read）
uv run python scripts/capacity/run.py --scenario tool_cycle \
    --levels 1,10 --cycle-s 0.2 --dwell 10 --warmup 2

# 落盘归因：同负载换存储后端（memory 不写 history.jsonl、不 fsync）
uv run python scripts/capacity/run.py --scenario tool_cycle --levels 10 \
    --cycle-s 0.2 --backend memory --out target/capacity/turn-cost-mem

# 内存轴：空转 + 历史体量
uv run python scripts/capacity/run.py --scenario idle --levels 1,10,100 --dwell 15 --warmup 0
uv run python scripts/capacity/run.py --scenario idle --levels 100 \
    --history-tokens 20000 --dwell 15 --warmup 0 --out target/capacity/idle-h20k

# 水合尖峰（重启网关后所有会话一起订阅）
uv run python scripts/capacity/run.py --scenario hydration --levels 100 \
    --history-tokens 2000 --dwell 20 --warmup 1

# 混合满占空比（70% 会话在流式输出，其余在做工具循环）
uv run python scripts/capacity/run.py --scenario mixed --levels 1000 \
    --rate 50 --chunk 4 --decode-duty 0.7 --cycle-s 3 --dwell 20 --warmup 5

# 同一件事经 make（旗标统一走 CAPACITY_ARGS，默认值只有 cli.py / shapes.py 一处）
make capacity CAPACITY_ARGS="--levels 1,10,50 --dwell 20 --json"
```

CLI（旗标名是下游脚本的契约，只允许追加）：

| 参数 | 默认 | 含义 |
|---|---|---|
| `--scenario` | `pure_stream` | 负载形态（见「负载形态」） |
| `--levels` | `1,10,50,100` | 逗号分隔的会话数阶梯（严格升序） |
| `--rate` | `50` | **每会话**注入速率，单位是**事件/s**（一个事件 = 上游 SSE 一个 delta 帧）。它是**注入参数**：Provider 睡够再写一帧，本机 `asyncio.sleep` 稳定多睡约 2 ms，所以运行前会空载实测这个超时并从帧间隔里扣掉；实际达到的速率以 `events_per_second_actual` / `chars_per_second` 为准。只有流式形态（`pure_stream` / `mixed` 的流式组）消费它：`tool_cycle` / `idle` / `hydration` 不按帧率驱动，给了也不生效 |
| `--chunk` | `1` | **每个事件携带的字符数**（`1` = 最坏情况；见「口径锚点」） |
| `--dwell` | `20` | 每档采样窗口（秒） |
| `--warmup` | `5` | 每档预热（秒，丢弃） |
| `--shards` | `4` | driver 分片子进程数（钳到 `min(shards, sessions)`） |
| `--subscribers` | `per-session` | `per-session` = 每会话一条 WS（贴近真实）；`per-shard` = 每分片一条（轻量档） |
| `--backend` | `file` | 会话存储后端（`POST /api/session/create` 的 `backend` 字段透传）：`file` 落盘、`memory` 进程内。用途是**同负载 A/B 出「每轮固定成本里落盘占多少」** |
| `--history-tokens` | `0` | 每会话预置的历史体量（token，估算值）。只有 `idle` / `hydration` 消费它（其余形态给了会报错，不静默忽略） |
| `--tool` | `read` | 工具循环用的工具：`read`（Read 一个 workspace 小文件，无子进程）/ `bash`（`Bash echo`，带子进程）。只有 `tool_cycle` 消费它 |
| `--cycle-s` | `3` | 工具循环的**周期下限**（秒）：`0` = 不 pacing，压到机器能吃下的最大轮次频率。只有 `tool_cycle` / `mixed` 消费它 |
| `--decode-duty` | `0.7` | 混合形态里处于纯流式输出中的会话占比。只有 `mixed` 消费它 |
| `--out` | `target/capacity/<UTC 时间戳>` | 产物目录 |
| `--criteria` | — | 打印判据原文（`verdict.CRITERIA`）后退出——阈值只在代码里，这个旗标是它的读取口 |
| `--selftest` | — | 离线自测 |
| `--json` | — | stdout 只输出一行 JSON 摘要（`ok` / `out` / `run_json` / `report` / `levels[]` / `failures`）；`levels[]` 每项是 `sessions` + `gateway_cpu_cores` / `delivery_p99_ms` / `tokens_per_second_actual` / `events_per_second_actual` / `chars_per_second` / `turns_per_second` / `errors` / `verdict` / `low_confidence` / `error` 的投影 |

退出码：`0` 全部档位完成；`2` 参数 / 环境错误（未启动任何进程）；`3` 有档位失败
（仍落盘产物，供取证）。`EMFILE` 之类「跑到一半才炸」的失败不会产出漂亮空表：
fd 上限在起任何子进程之前就抬好，抬不动直接退出（见「fd 上限」）。

**形态专属旋钮的适用范围由形态自己声明**（各形态模块的 `KNOBS`）：给某个形态一个它不消费的
旋钮（例如 `pure_stream --tool bash`）是**参数错误**，不是静默忽略——静默忽略会让人
以为「那个变量已经测过了」。

## 口径锚点

- 负载的基本单位是**事件**：上游 SSE 一个 delta 帧 = 网关一次 `emit` = 一条 WS 帧。
  `--rate` 是**每会话事件/s**，`--chunk` 是每个事件的字符数，帧间隔恒为 `1/rate`
  （`--chunk` 不改帧数，只改每帧的字节数）。
- **`--chunk=1` 是上界（最坏情况）**：每次唤醒只喂一个字符。真实 provider 每个 delta
  常见 1~20 字符，所以 `--chunk=4…8` 更接近 OpenAI / Anthropic 的真实分帧。两档都要跑，
  报告里分开写：`chars_per_second` 是实测的（分片对 `text` 事件的 `content` 长度求和），
  不是 `events × chunk` 反推——反推会把「网关丢了事件」洗成「字符数正常」。
- 1000 会话 × 50 事件/s = 50k 事件/s，这是「满速」的定义。

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
接近核数 ⇒ 这一档测的是 rig，不是网关，判据把它标成 `low_confidence`。

三个 CPU 数字是**同一个窗口口径**（窗口起止的累计 CPU 差 / 窗口秒）：网关由 coordinator
外部采样，Provider 与分片在各自进程里自记账（起点钉在 `window_start`，收到窗口消息时
并不起算——那时还在 warmup）。采网关的 RSS / fd 要走 `ps` / `lsof` 子进程（合计约 17 ms），
所以放在线程里跑：同步跑会每秒卡住一次正在产出负载的 Provider，把抖动记进读数。
形态可以覆盖采样间隔（水合形态用 0.25 s——1 s 的峰值分辨率会把尖峰整个错过）。

**一档里除了采样窗口还有三步预处理**，它们都不是被测变量，但都会在产物里留痕：
建会话（`session_creation`，本身是被测数字）→ 历史预置（`history.*`，只在
`--history-tokens > 0` 时跑，一个 seed 分片）→ 网关重启（`gateway.restart_ms`，只在
`hydration` 形态跑）。三步都在采样窗口之前完成，窗口里只有测量负载。

## 产物

```
<out>/
├── run.json                       # 机器可读（字段名冻结，只新增）
├── report.md                      # 人读：结论 + 表格 + 判据 + 复现命令
└── raw/<N>/                       # 每档原始样本
    ├── shard-<k>.samples.json     # 该分片的每次轮次 + 每帧到达时刻（相对窗口的微秒）
    ├── shard-<k>.json             # 分片配置（会话分配 / 订阅模式 / 窗口模式）
    ├── shard-<k>.log              # 分片 stderr（崩溃取证）
    ├── gateway.log                # 该档网关日志（崩溃 / 断连取证用）
    └── seed/shard-0.*             # 历史预置阶段的分片（只在 --history-tokens > 0 时）
```

`run.json` 的冻结字段（**取不到就是 `null`，绝不写 0**）：

```jsonc
{
  "meta": {
    "tool": "capacity", "schema": 1, "scenario": "pure_stream",
    "started_at": "<ISO8601Z>", "duration_s": 123.4,
    "args": {"levels": [1,10], "rate": 50.0, "chunk": 1, "dwell": 20.0, "warmup": 5.0,
             "shards": 4, "subscribers": "per-session", "backend": "file",
             "tool": "read", "cycle_s": 3.0, "decode_duty": 0.7, "history_tokens": 0,
             "frame_delay_ms": 17.945, "sleep_overshoot_ms": 2.055},
    "host": {"cpus": 12, "mem_gb": 16.0, "platform": "darwin", "ulimit_n": 4256},
    "gateway": {"pid": 123, "version": "0.7.0", "commit": "a7e7126"},
    "session_create": {"ok": 11, "failed": 0, "total_ms": 25.5, "scope": "sum over levels"},
    "notes": ["…"], "failures": [], "out": "<out dir>"
  },
  "levels": [{
    "sessions": 10, "wall_s": 5.08,
    "metrics": {
      "gateway.cpu_cores": 0.117, "gateway.rss_mb_peak": 73.2,
      "gateway.rss_mb_end": 73.2, "gateway.rss_mb_slope_per_session": 0.3,
      "gateway.fds_peak": 47,
      "delivery.p50_ms": 2.432, "delivery.p99_ms": 3.117, "delivery.p999_ms": 3.357,
      "turn.p50_ms": null, "turn.p99_ms": null,
      "hydration.p50_ms": null, "hydration.p99_ms": null, "hydration.p999_ms": null,
      "first_frame.p50_ms": null, "first_frame.p99_ms": null, "first_frame.p999_ms": null,
      "tokens_per_second_actual": 490.2, "events_per_second_actual": 490.2,
      "turns_per_second": 0.0, "chars_per_second": 490.2,
      "errors": 0, "slow_client_recycles": 0
    },
    "rig": {"provider_cpu_cores": 0.029, "shard_cpu_cores": [0.018, 0.015, 0.012, 0.013],
            "host_loadavg": [4.32, 5.26, 4.74]},
    "samples": {"count": 2491, "dropped": 1039,
                "dropped_reasons": {"outside_window": 995, "in_flight_tail:content": 44},
                "delta_frames": {"content": 2491, "thinking": 0, "tool_call": 0},
                "text_frames": 2491, "chars": 2491,
                "turns": 0, "turns_in_flight": 10,
                "delivery_available": true,
                "sync_sessions": 0, "hydration_attempts": 0, "hydration_failed": 0,
                "evicted_by_reaper": null, "released_by_probe": null},
    "history": {"tokens_per_session": null, "chars_per_session": null,
                "turns_per_session": null, "total_ms": null, "failed": null},
    "session_create": {"ok": 10, "failed": 0, "total_ms": 13.9},
    "gateway": {"pid": 456, "url": "http://127.0.0.1:63665",
                "version": "0.7.0", "commit": "a7e7126", "restart_ms": null},
    "shape": {"rate": 50.0, "chunk": 1, "backend": "file", "tool": "read",
              "shards": 4, "subscribers": "per-session", "window_mode": "drive"},
    "verdict": {"verdict": "ok", "flags": [], "reasons": [], "low_confidence": false,
                "low_confidence_reasons": [], "baseline_sessions": 1},
    "notes": ["…"]
  }]
}
```

**成功档与失败档的键集完全相同**（同一个 `level_skeleton()`）：失败档只是多一个 `error`
字符串、值全为 `null`（含 `verdict.verdict`）——下游按冻结字段名直取不会 `KeyError`。

追加字段（不改上面任何冻结名，读方可以忽略）：

- `meta.notes` / `meta.failures` / `meta.out` / `meta.selftest`：全局说明、失败摘要、
  产物目录、自测标记；
- `meta.args.frame_delay_ms` / `sleep_overshoot_ms`：本次运行实测的 `asyncio.sleep` 超时
  与其补偿结果（`--rate` 是注入参数，见「已知边界」）；
- `meta.session_create` 是**全档汇总**（`scope: "sum over levels"`）——逐档数字在
  `levels[i].session_create`；有档位没拿到数字（环境都没起来）时整组写 `null` + `scope`
  说明，不按 0 求和；
- `levels[i].gateway` 是本档网关身份（每档都是新进程；水合形态重启后是新 pid / 新端口，
  `restart_ms` 是那次优雅重启的耗时，其余形态为 `null`）；
- `levels[i].shape` 是本档的形态参数（`chunk` / `backend` / `tool` / `window_mode` 都在这里，
  否则 `file` 与 `memory` 两次读数在产物里分不开）；
- `levels[i].history` 是历史预置的记账（`total_ms` 是**预置花了多久**）；
- `levels[i].verdict` 是判据结果（`verdict.py` 的唯一实现），报告**不用**它、而是自己重算
  一遍（见「判据」）；
- `levels[i].samples.chars` 是窗口内收到的正文字符数（`chars_per_second` 的分子，
  实测值）；`sync_sessions` / `hydration_attempts` / `hydration_failed` 是水合的三个计数；
  `evicted_by_reaper` / `released_by_probe` 是**窗口末时刻**的逐出探针结论（只在 `idle`
  形态非 `null`：前者 = 探针到达时已经不在内存，后者 = 还在内存、被探针放掉）；
- `levels[i].error_detail`：该档失败的完整诊断（网关日志尾部等）。一句话版本在 `error`。

## 口径

| 指标 | 定义 |
|---|---|
| `gateway.cpu_cores` | 窗口内网关进程 CPU 秒 / 窗口秒（首尾两次**累计值**之差；不做周期采样，否则采样器自己的开销会混进来） |
| `gateway.rss_mb_peak` / `rss_mb_end` | 窗口内按 `sample_interval` 采样的峰值 / 末次值（Linux `/proc/<pid>/statm`，macOS `ps -o rss=`） |
| `gateway.fds_peak` | 同上频率采样的 fd 数峰值（Linux `/proc/<pid>/fd`，macOS `lsof -p`）——峰值分辨率 = 采样间隔 |
| `gateway.rss_mb_slope_per_session` | 与**上一档**末次 RSS 的相邻差斜率；最小档无上一档 → `null` |
| `delivery.*_ms` | 客户端收到第 i 帧的时刻 − 假 Provider 发射第 i 帧的时刻（同机 `time.monotonic()`）。**按通道**配对：`content`→`text`、`thinking`→`reasoning`、`tool_call`→`tool_call_stream`（见「投递延迟的配对」） |
| `turn.*_ms` | 发 prompt → 收到 `turn_result`（只取窗口内完成的轮次）。`error` 事件不是轮次边界——它是失败轮的伴随事件 |
| `hydration.*_ms` | 订阅请求发出 → `sync_session` 到达（读 history + 拼快照 + 过 wire）。只有水合形态有值 |
| `first_frame.*_ms` | 驱动 prompt 发出 → 首个正文增量（水合之后第一次真实调用）。只有水合形态有值 |
| `tokens_per_second_actual` | 窗口内收到的 `content` delta **事件**数 / 窗口秒（冻结的历史名字，**不是 token**）。字符口径看 `chars_per_second`；token 只能按产品估算器从字符反算 |
| `events_per_second_actual` | 窗口内收到的**全部通道** delta 事件数 / 窗口秒。窗口里一个事件都没收到时是 `null`（未测到），不是 0 |
| `chars_per_second` | 窗口内收到的正文字符数 / 窗口秒（实测；`--chunk` 的直接体现）。与 `events_per_second_actual` 同口径：没有事件时是 `null` |
| `turns_per_second` | 窗口内**完成**的轮次数 / 窗口秒。`cpu_cores / turns_per_second` = 每轮固定成本。`pure_stream` 结构性为 0（窗口里没有轮次边界） |
| `errors` | 窗口内（事件到达时刻在 `[window_start, window_end]` 内）的 `turn_result.is_error` 数 + 分片发送失败数。失败轮上网关会**同时**发 `turn_result(is_error)` 与 `error` 事件，只记前者的原因见 :mod:`driver_shard` |
| `slow_client_recycles` | 窗口结束前就被网关回收的客户端连接数（分片在 `window_end` 时刻数一遍自己的连接：此刻还没关连接，`closed` 的都是网关先断的）。不扫网关日志 |
| `rig.provider_cpu_cores` | coordinator 进程（= Provider 宿主 + 采样器）在窗口内的 CPU 秒 / 窗口秒；上界，不是纯 Provider |
| `rig.shard_cpu_cores[]` | 每个分片**自报**的窗口内 CPU 秒 / 分片自己的记账区间（起点钉在 `window_start`，因此与上面两个同口径、可相加）；分片上报的区间与采样窗口差 >10% 时留一条 note |

百分位是 **nearest-rank**（p99 = 升序第 `ceil(0.99·n)` 个值，不插值），与
`scripts/perf/suite_gateway.py` 一致；实现只在 `metrics.percentile()` 一处。

### 投递延迟的配对（按通道，严格，不猜）

1. driver 每次发的 prompt 文本全局唯一（`load:<session>:<seq>`；预置历史用
   `seed:<session>:<seq>`，永不与驱动轮撞键）；
2. coordinator 在假 Provider 的 `RequestLog` 里按「**最后一条 user 消息正文** == 该文本」
   定位 `request_index`（`content` 可能是 block 列表，取其中的 `text` 块拼接）；
3. 上游给每帧一个**通道标记**（`FrameEmit.kind`，闭合词表 `sse.FRAME_KINDS`）。配对逐
   通道进行，通道之间互不影响：

   | 通道 | 客户端事件 | 说明 |
   |---|---|---|
   | `content` | `text` | 正文增量（1 帧 = 1 个事件） |
   | `thinking` | `reasoning` | 推理增量 |
   | `tool_call` | `tool_call_stream` | 工具参数增量 |
   | `meta` | —（**不配对**） | role 空帧 / finish 帧 / usage 帧 / `[DONE]` |

4. **一个 prompt 命中多个请求时按通道消歧**：工具循环的一轮是「调用轮（发
   `tool_call` 帧）+ 文本轮（发 `content` 帧）」两次请求，同一 prompt 因此对应两条留档；
   而某个通道的帧只可能来自发过该通道帧的那一次请求。仍有多个候选（网关重试）才整轮
   丢弃（`ambiguous_prompt`）——不猜；
5. 同一通道内按下标对齐（第 i 帧 ↔ 第 i 个事件），取两者较短的长度做前缀；余下的按方向
   记账，绝不猜：已完成轮次的发射侧富余 → `missing_events:<kind>`（客户端少收，异常）、
   窗口末仍在飞的轮次 → `in_flight_tail:<kind>`（预期）、到达侧富余 →
   `unexpected_events:<kind>`（客户端多收）。落点不在采样窗口内的配对（跨窗口起点的预热段）
   记 `outside_window`；负延迟记 `negative_latency`；prompt 未命中 / 命中多条（重试）
   分别记 `unknown_prompt` / `ambiguous_prompt`。

丢弃的帧按原因计数进 `levels[i].samples.dropped_reasons`（**单位是帧**，
`sum(reasons) == dropped`），并写进 `notes`；`samples.count` 是进入延迟分布的帧数。
`delivery_available` 在失败档是 `null`（未知），不是 `false`。

## 负载形态

五种形态各回答一类问题。共同契约见 `shapes.py`：`NAME` / `MODEL` /
`plan(args) -> ShapePlan`（要注册哪些 model、每会话挂哪个 model、工具集、workspace 预置
文件、窗口模式……形态的全部语义都在这一个返回值里）。`prompt_text` 可选：不声明即用
`shapes.prompt_key`（`load:<session>:<seq>`）。

形态参数（`ShapeArgs`）里没有「workspace 路径」：工具调用的参数是**相对字面量**
（`Read` 的 `path` = `note.txt`），由网关按会话的 workspace 解析——形态不需要、也不应该
自己拼绝对路径。

### `pure_stream` — 探并发边界的主负载

每会话**一轮覆盖整个档位时长**（`rate × (warmup + dwell)` 个事件 + 1 s 余量），帧间隔
`1/rate`。于是采样窗口里每个会话都在同一轮之内：

- 窗口内**没有轮次边界** ⇒ `gateway.cpu_cores` 只含「每事件成本」，可以按会话数扫出这个常数；
- 每轮固定成本（上下文装配 / 落盘 / 审计 INSERT / 轮次事件）**不在**这个形态的读数里，
  要看 `tool_cycle`；`samples.turns` 恒为 0、`turns_in_flight` 等于会话数——这就是证据；
- `turn.*` 结构性是 `null`（没有完成的轮次），报告里有一行 note 说明，不是没测到；
- 代价（内存）：Provider 会为**每个在飞请求**物化整条帧序列，内存与
  `sessions × rate × span` 同阶（每帧字节数再乘 `chunk`）。1000 会话 × 50 事件/s × 25 s
  约 10^6 帧，跑大档前先确认机器吃得下。

### `tool_cycle` — 每轮固定成本

每会话循环「一轮」：客户端发 prompt → 模型吐一个工具调用 → 网关执行工具 → 第二次模型
调用吐一行正文 → `turn_result`。`cpu_cores / turns_per_second` 就是**每轮固定成本**
（提交 + 落盘 fsync + 审计 INSERT + 下一次请求组装），它与每事件成本差两个数量级。

- `--tool read`（默认）读 workspace 里的小文件；`--tool bash` 用 `Bash echo`（带子进程 +
  进程组收尾）。bash 变体的会话在 create 期带 `agent.yolo=true`：`safe_command_patterns`
  为空时任何 Bash 命令都要人确认，而 headless 测量里没有人回答（会挂在 6000 s 的
  feedback 等待上）。
- `--cycle-s` 通过「调用轮首个内容帧之前的 `first_delay`」把周期撑到**不短于**它：
  周期 = `cycle_s` + 该轮的真实成本，所以 pacer 不污染每轮成本的读数；`--cycle-s 0` =
  不 pacing。**不 pacing 时两个会话就能把一个核吃满**（每轮 ~9 ms CPU × 100 轮/s），
  那一档按判据就是 `capped`——测「最大轮次频率」时这是预期结果，不是 bug。
- **每会话一个 model id**（`capacity/tool_cycle/s<k>`）：假 Provider 的剧本游标按 model
  走，而「调用轮 → 文本轮」两段的类型不同；共享游标时会话之间一旦漂移一个请求，某个会话
  就会把文本轮当成调用轮，工具不再执行、轮长变成两种的混合。

### `idle` — 内存底账（含 reaper）

建 N 个会话后什么都不驱动，只采 RSS / fd 与相邻档的每会话内存斜率。配合
`--history-tokens` 就是内存轴的另一端。两个刻意的选择：

- **不建任何客户端连接**（`window_mode=hold`）：订阅会钉住会话（产品的逐出条件之一），
  那 reaper 就永远不会动手，窗口里也就观察不到逐出；
- **把逐出阈值压进窗口**：产品默认 `sessions.eviction.idle_ttl_seconds` 1800 s / 扫描
  300 s，秒级窗口里什么都看不到（等于没测）。会话的空闲计时从**建会话**起算（建会话与
  历史预置都在窗口之前），所以阈值取 `warmup + dwell/2` 与 1 s 扫描：早于窗口结束
  （`warmup + dwell`）而晚于窗口起点（`warmup`），于是窗口里应当发生一次批量逐出；
- 「逐出到底发生了没有」不靠 RSS 猜：窗口结束后对每个会话调
  `POST /api/session/release`（公开路径、幂等），`released=false`（`not loaded`）就是
  「reaper 已经回收了」。两个计数进 `samples.evicted_by_reaper` / `released_by_probe`
  （**窗口末时刻**的快照，不含逐出发生的时刻）；`memory` 后端的会话不允许逐出（产品
  禁止销毁非持久数据，409），此时两个计数写 `null` + note，不写 0。
- **RSS 落差不是逐出的证据**：进程被清空后分配器一般不还页，实测「50 个会话全被逐出」
  时 `rss_mb_peak == rss_mb_end`。要不要相信逐出，只看 `/release` 探针。

### `hydration` — 网关重启后的水合尖峰

建 N 个会话（可带历史）→ 优雅重启网关（`ProbeEnv.restart_gateway()`）→ 分片在窗口起点
**同时**订阅全部会话 → `sync_session` 到达 → 发一条 prompt → 首个正文增量。三段时间各自
成为指标、不混成一个「启动耗时」：`hydration.*`（水合）、`first_frame.*`（水合后第一次
真实调用）、`turn.*`（同别的形态一个口径）。重启耗时单独记 `gateway.restart_ms`。

订阅时**不设并发闸**：尖峰本身就是被测对象，串行订阅会把一条平滑的曲线测成人工的低谷。
`--backend memory` 与本形态互斥（进程内会话不过重启，水合必定 404），CLI 直接拒。

### `mixed` — 模型校验

`--decode-duty`（默认 0.7）= **处于纯流式输出中的会话占比**：前 `round(duty × N)` 个会话
跑 `pure_stream`（一轮覆盖整窗），其余跑 `tool_cycle`。于是「混合档 ≈ 两种纯形态读数的
线性叠加」是一个可检验的预测；偏差大就说明某个成本项不与会话数线性（cache 抖动、GIL、
页回收），必须人工解释而不是当噪声。选**会话分组**而不是时间片：时间片下每个会话都在
两种成本之间来回切，叠加预测不再成立，本形态就失去了唯一的用途。

### 历史预置（`--history-tokens`）

只对 `idle` / `hydration` 有效。每会话发一条 prompt，假 Provider 一次吐出
`history_tokens × chars_per_token` 字符（`chars_per_token = 1 / TokenCounter.NON_CJK_RATIO`，
直接引用产品估算器的比例，不复制数值），按 4000 字符/帧分帧——**一轮、一条 assistant
记录**。

- 为什么不是「多轮小输出」：轮数是**每轮固定成本**的乘数，多轮会把「预置历史」变成
  「预热一遍每轮固定成本」，几十倍放大预置耗时；而水合成本主要由字节数决定。
- 为什么不是「直接构造 `history.jsonl`」：会话创建后内存态已被网关持有，向磁盘追加记录
  既不会进入内存链、也会在下次提交时被覆盖——那就不再是产品读得到的规范形了。预置走
  `POST /api/session/subscribe` + `/ws` 驱动 + `turn_result` 收口，与真实客户端逐字同路径。
- 预置轮挂**另一个 model**（`capacity/seed`）：会话在 create 期用 `agent.model_id` 指向它，
  预置完再用 `POST /api/session/update` 切回形态自己的 model。预置轮与测量轮各有独立剧本
  游标，不会因为重试或时序把「大输出轮」喂给测量阶段。
- 预置耗时照实记进 `levels[i].history.total_ms`（`--history-tokens 20000` × 1000 会话是
  分钟级的活，必须让人看得见）。

## 判据（`verdict.py`）

判档是「看图说话」的反面：每个结论都要指回一条写死的判据原文。**阈值与判据原文只在
`verdict.py` 声明一处**（`CRITERIA`），报告逐条打印它，本文**不复制数字**——复制就会出现
第二份事实来源。要看原文：

```bash
uv run python scripts/capacity/run.py --criteria
```

口径要点（数字见上）：

- `baseline` = 阶梯里**最小 N** 那一档的同名指标。基线档没跑完 ⇒ 退化判据无从评估，
  该档 `verdict=unknown`（**不是** `ok`）——「跑挂了」不许被读成「跑得很好」。
- `verdict` ∈ `ok` / `degraded` / `capped` / `unknown`：`capped` 优先于 `degraded`
  （同一档两者都命中时，天花板是更强的结论），`flags[]` 列出全部命中。
- `degraded`：投递延迟的尾部越界（相对基线，带一个绝对地板）或多轮形态的轮次尾部越界。
  轮次判据**只在 `tool_cycle` / `mixed` 判**（其余形态的窗口里没有轮次边界）。
- `capped`：网关 CPU 打满 / 出现轮次错误 / fd 接近 `meta.host.ulimit_n`。
- `low_confidence`（**不进判档**，但报告必须点名）：rig 自身饱和（provider 或某个分片
  吃满核）、**不预期**的配对丢弃比例越界、建会话失败。这四条都意味着「这一档的读数可能
  是 rig 自己造成的」，所以报告的结论句**不引用**这些档位。
- `unjudged`（第四种状态，也不进判档）：某条判据**无从评估**——今天只有一条，多轮形态的
  `turn.p99` 没有可比的基线（基线档没有完成的轮次）。它与 `low_confidence` 分开：读数本身
  是干净的，只是少了一条判据，报告照常用其余判据下结论。
- 丢弃比例只数**不预期**的原因：`samples.dropped` 含 `outside_window`（跨窗口起点的预热段）
  与 `in_flight_tail:*`（窗口末仍在传输）——两者都是采样窗口的簿记，不是数据被污染。
  `pure_stream` 的整轮本来就跨进窗口起点，按总数算会让**每一档**都「低置信」，那等于
  没有信号。哪些原因算簿记由 `metrics.DROP_REASONS` 一张表声明（生产者与判据共用，
  改一个名字只有一处要改）；`dropped_reasons` 缺失（手写产物）时无法归类，按全部不预期
  处理（保守）。
- 报告**重算**判据（`verdict.evaluate_run`），不读 `levels[i].verdict`：那份投影是给机器
  读的，而报告还要能处理手写的 / 被污染的 `run.json`——没有 `verdict` 字段的输入同样必须
  被标低置信。

## 报告（`report.md`）

依次是：横幅（失败档 / 投递延迟不可用 / **低置信** / 无法判定）→ 元信息（args / host /
gateway）→ **结论**（速度轴 = 判据命中的第一档；内存轴 = RSS 斜率 × 1000 会话的线性外推，
只做投影、不判档）→ 阶梯表（含 `verdict` 列）→ 水合与首帧表（有数据时）→ rig 健康 →
notes → **判据原文 + 逐档命中** → 复现命令 → 口径脚注。

## fd 上限

每个并发会话大致吃 4 个 fd（一条 WS + 一条上游 HTTP 连接，再留一倍余量）。默认软限
（本机开发 shell 常见 256）在几十个会话上就会 `EMFILE`，而 `EMFILE` 出现在窗口中途时
产出的是一张「好看的空表」——所以：

1. `run.py` 在**起任何子进程之前**把 `RLIMIT_NOFILE` 软限抬到
   `max(当前软限, 4 × N_max + 256)`（封顶硬限），子进程继承；
2. 实际生效值写进 `meta.host.ulimit_n`（`capped` 的 fd 判据以它为分母）；
3. 抬不到目标就带可行动的错误退出（附当前 / 目标 / 硬限），而不是跑一半才炸。

`make capacity` 目标另有一层 `ulimit -n 8192`（`CAPACITY_NOFILE` 可覆盖）。

## 为什么不能并行跑两次

同一台机器上的两次测量互相抢 CPU / 内存带宽 / 页缓存，读数会被对方污染，而 `rig` 上报
（provider + 分片 CPU）只能看见自己那一半。所以：**一次只跑一个 `run.py`**，跑之前把别的
任务停掉，档位之间不要穿插编译。同一命令重跑给出同量级数字（随机性——session id、端口——
不影响读数）；跨机器不可比。

## 与 scripts/perf、wing-probe 的关系

- `scripts/perf/` 用同一套「真网关 + 假 Provider」驱动**一个**会话做 A/B 回归；
  容量工具驱动 N 个会话做**绝对值**扫描。两者都不改产品、都不碰 CI 门禁。
- 进程采样（CPU 的 darwin / linux 分支）与百分位秩定义**照搬** perf 的实现与口径，
  但各有各的一份：那边住在一个套件脚本里（不是共用模块），跨工具 import 会把整条
  perf 测量链拖进来。**origin 是 perf 那一份**（更早、被 CI 覆盖）——改任何一边都要同步
  另一边，本目录的实现是派生。第三处同款是原子写 JSON（`run.write_json` vs
  `perf/common.write_json`，十行）。场景装配（进程拓扑 / 采样窗口 / 分片）是本工具独有，
  与 perf 的「单会话 A/B」不是同一件事，不复用。
- `libs/wing-probe/` 是本工具的底座（`ProbeEnv` 起环境 / `restart_gateway`、`FakeProvider`
  当上游、`GatewayWS` 当传输层参考实现）。分片为什么不用 probe 的 `Driver` / `Session`：

  1. `Driver` 用**一条** WS 承载它挂载的全部会话（会话靠事件里的 `session_id` 路由），
     撑不起 `--subscribers per-session`（每会话一条 WS）这个默认拓扑；
  2. `Session.chat()` 是「发一条 → 等整轮」的阻塞调用，没有「一轮结束立刻发下一条、
     直到窗口结束」的泵；
  3. 会话由 coordinator 统一建（建会话耗时是被测数字），与 `Driver.session()` 的职责冲突。

  也不用 probe 的 `Timeline`：它永久保留每个事件，1000 会话 × 50 帧/秒 会让分片先 OOM
  （分片的原始帧日志有界保留 64 条，只作诊断）。

## 自测（`--selftest`）

≤120 秒（当前约 32 s）、不联网、不需要 API key、可重复。它起真网关子进程，因此和
`scripts/perf/ab.py --selftest` 一样**不进** `make check`（那一条只跑静态检查）——
改完这个工具先手动跑 `make capacity-selftest`：

| 检查 | 证明什么 |
|---|---|
| `stats.percentile` | 百分位秩定义的 known answer（含 n=1 / 偶数 / 极值 / 空序列报错） |
| `pairing.last_user_text` | 两种 body 形态（str / block 列表）都能取到配对键 |
| `pairing.delivery` | 配对数学 + 丢弃原因 + **同一 prompt 多请求时按通道消歧** + 缺接口抛错 |
| `pairing.assemble` | 配对结果与新增指标（`chars_per_second` / `turns_per_second` / 水合两段时间线）落到 `run.json` 的接线 |
| `shard.events` | 分片的事件处理：失败轮只驱动一轮、字符实测、水合只在 `sync_session` 之后驱动一轮 |
| `proc.sampling` | 自身 CPU 单调、RSS > 0、fd ≥ 3、fd 软限抬升到位 |
| `cli.contract` | 冻结 CLI 的默认值与全部校验分支（含新增旗标与形态专属旋钮的适用范围） |
| `shapes.contract` | 五种形态的 plan 契约：会话 model 归属、chunk、窗口模式、workspace 文件、预置剧本循环 |
| `verdict.criteria` | 判据：baseline / degraded / capped / unknown / low_confidence 与「结构性丢弃不算污染」 |
| `verdict.polluted` | **负向验证**：被污染的 `run.json`（rig 饱和 / 不预期丢弃）必须被标低置信 |
| `report.render` | 合成 `run.json` → markdown（判据原文唯一、缺数据是 `—`、复现命令、表格列数） |
| `run.summary` | `--json` 一行摘要的字段名与取值投影（下游脚本的契约） |
| `e2e.<case>` | **真网关子进程 + 真分片子进程**跑完迷你一档（六个用例：五种形态 + `--tool bash` 变体）：断言该形态的指标非 null 且形状自洽（`pure_stream` 无轮次边界、`tool_cycle` 有完成的轮次与 tool_call 帧、`idle` 无事件且 reaper 逐出全部会话、`hydration` 有三段时间线、`mixed` 两个通道都有帧），并断言真实 `dropped_reasons` 全部落在 `metrics.DROP_REASONS` 词汇表里；再用一个失败档断言「失败档键集 == 成功档键集」、一个相对 `--out` 档断言默认形态能跑 |

`--tool bash` 那条值得单独存在：`yolo` 一旦失效，`Bash` 会落到「等人确认」上（headless
测量里没有人回答），轮次从此永不收口——这条回归只有真跑一次才看得见。

`e2e` 走的是产品代码同一路径（注入 `run_ladder`），不是平行实现。

## 已知边界

- **投递延迟依赖上游接口**：`FakeProvider.frame_emits`（逐帧发射时刻 + 通道标记）。
  接口缺席时 `delivery.*` 与 `samples.count/dropped` 都是 `null`，`samples.delivery_available=false`，
  且 `meta.notes` / `levels[i].notes` / `report.md` 顶部三处点名——**不写 0、不估计**。
  其余指标（CPU / RSS / fd / 帧速率）照常可用。
- **`--chunk=1` 是上界**：它是「每个 delta 只带一个字符」的最坏情况。真实 provider 每个
  delta 常见 1~20 字符，`--chunk=4…8` 更接近现实；同一份负载两档的 `gateway.cpu_cores`
  差别（每事件成本基本相同、每字节成本增加）就是 chunk 的代价。
- **速率的两段误差**（`--rate` 只是注入参数）：
  1. Provider 的节奏是「睡够再写一帧」，本机 `asyncio.sleep(20ms)` 实测稳定多睡约
     2.1 ms（与 delay 无关）⇒ 不补偿时 50 事件/s 只能跑到约 45.6。运行前空载实测这个
     超时并从句间隔里扣掉（`meta.args.sleep_overshoot_ms` / `frame_delay_ms`）。
  2. 残余误差与机器负载、网关消费速度有关，仍以实测的 `events_per_second_actual` /
     `chars_per_second` 为准；`delivery` 的 `in_flight_tail` 会告诉你「发射了但窗口末
     还没到客户端」的那几帧。
- **`turn.*` 对 `pure_stream` 是 `null`**（单轮形态的结构性结果）；想看轮次耗时用
  `tool_cycle` / `mixed`。窗口内轮次数照样上报（`samples.turns` / `turns_in_flight`），
  因此「每轮固定成本」可以由「多轮形态 − 单轮形态」或 `cpu_cores / turns_per_second` 得到。
- **`idle` 的逐出是「压进窗口」的**：它的读数回答「N 个空闲会话的常驻内存 + reaper 会不会
  在窗口里回收」，**不是**产品默认阈值（1800 s）下的行为。默认阈值下窗口里不会有逐出，
  `rss_mb_end == rss_mb_peak` 既可能是「没逐出」也可能是「逐出了但没还页」——
  逐出与否只看 `/release` 探针的计数。
- **`mixed` 的线性叠加是近似**：两组会话共享同一个网关进程（事件循环、内存分配器、
  socket 缓冲都共享），偏差大就要人工解释而不是当噪声。
- **`--history-tokens` 的 token 是估算值**（产品估算器 `TokenCounter.NON_CJK_RATIO`），
  真实字节数看 `history.chars_per_session`。
- 订阅模式不同，单会话成本不同：`per-session` 是 N 条 WS（贴近真实，开销更大），
  `per-shard` 是 K 条（轻量档，仅用于自测 / 极限档）。两者读数不可直接对比。
- 一档内所有会话共用一个 workspace 目录（工作目录不影响被测变量）；成功档收档时删掉，
  失败档保留（`raw/<N>/workspace`）以便复盘。
- 会话工具集只有形态用到的那一个（`Read` 或 `Bash`）：真实部署的工具 schema 与 system
  prompt 更大、会摊进每轮请求，因此这里量到的是**最小工具集下的上界**（会把容量报高）。
- fd 峰值与 RSS 峰值的分辨率是采样间隔（默认 1 s，水合形态 0.25 s），短尖峰可能采不到。
- 一档的全部分片样本会一次性读进 coordinator 内存再配对（1000 会话、每会话数轮、
  每轮数百帧的量级约几十 MB）——这是「配对要跨进程读两边」的代价，不是可省的开销。
- 分片崩溃 / 网关提前退出会让该档记 `error`、**未测到的**指标为 `null`（已经测到的照常
  输出：一个分片崩掉不影响其余分片与网关侧读数），**其余档继续**跑（退出码 3）。

## 排错：症状 → 先看哪里

| 症状 | 先看 |
|---|---|
| 某档指标全 `null` | `levels[i].error`；`raw/<N>/gateway.log`、`raw/<N>/shard-*.log` |
| `delivery.*` 全 `null` | `samples.delivery_available`、`meta.notes`（上游发射时刻是否就位） |
| `samples.dropped` 很大 | `samples.dropped_reasons`：`outside_window` = 跨窗口起点的预热段（正常）；`in_flight_tail:<kind>` = 窗口末仍在传输（正常）；`missing_events:<kind>` = 客户端少收了事件（异常）；`unexpected_events` = 客户端多收（异常）；`ambiguous_prompt` = 网关重试 |
| 报告说「低置信」 | 该档 `verdict.low_confidence_reasons`：provider / 分片 CPU 吃满 ⇒ 加大 `--shards` 或降 `--rate`；不预期丢弃 ⇒ 看 `dropped_reasons` |
| `events_per_second_actual` 明显低于 `--rate` | `meta.args.sleep_overshoot_ms` 是否接近帧间隔（补偿失效）；`in_flight_tail` 是否偏大（网关消费不过来） |
| `turn.*` 是 `—` | 形态决定：`pure_stream` / `idle` 结构性为 null；`tool_cycle` / `mixed` 为 null 说明窗口内没有轮次收口（`--cycle-s` 太长或网关卡住） |
| `hydration.*` 是 `—` | `samples.hydration_attempts` / `hydration_failed`；分片是否在窗口起点才订阅（`shape.window_mode == "hydrate"`） |
| `evicted_by_reaper` 是 `null` | 逐出探针被拒（`memory` 后端 409 / 会话被钉住）；note 里有首条失败原因 |
| `verdict` 是 `unknown` | 基线档（最小 N）没跑完，退化判据无从评估 |
| 网关 CPU 高但 rig 也高 | 加大 `--shards`；看 `rig.shard_cpu_cores` 是否有个别分片吃满 |
| `EMFILE` / `Too many open files` | `meta.host.ulimit_n` 是否达到 `4 × N + 256`；见「fd 上限」 |
| 读数在两档之间抖动 | `rig.host_loadavg`（机器是否被别的任务占用）；容量测量必须串行跑 |
| 分片报 `FileNotFoundError: ... shard-0.json` | 产物目录不是绝对路径（`RunConfig` 会规范化；自己构造 `RunConfig` 时别绕过它） |
| `--history-tokens` 报参数错误 | 只有 `idle` / `hydration` 消费它（形态模块的 `KNOBS`） |
