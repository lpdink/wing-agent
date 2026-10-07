# PR 级性能回归测量（perf-ci）

每个 PR 自动跑一轮"用户真实热路径"的性能测量，以 **merge-base** 为对照、**同一次 run 内
交错 A/B**，结果贴成 PR 上的一条 sticky comment（`<!-- wing-perf -->`）。**不做门禁**：
job 只在基础设施故障时红（构建/测量失败、报告渲染失败），性能回归只用评论表达。

- 代码：`scripts/perf/`（`ab.py` 编排 · `common.py` 契约 · `report.py` 渲染 ·
  `comment.py` upsert · `suite_{rust,tui,gateway}.py` 三个套件 · `thresholds.json`）
- CI：`.github/workflows/perf.yml`（PR 触发 + 手工 dispatch）；评论正文也喂给
  `.github/workflows/wing-review.yml` 的 AI 审查者（它把与改动相关的回归点并入结论）
- 速查表：`scripts/perf/README.md`

## 1. 方法论

**基线口径 = merge-base，同 run 交错 A/B**。不维护跨 run 的历史基线（runner 换了、
机器换了、today 的邻居也换了，"上周三的绝对值"没有可比性）。每次 run：

1. `git worktree add --detach <workdir>/base <merge-base>`：base 侧是**独立 worktree +
   独立构建产物 + 独立 `WING_HOME`**，与 head 侧只允许"代码来源"不同。
2. 逐 suite 逐轮交错：`for r in 1..rounds { base r; head r }`。
3. 聚合：每个指标先取"每轮代表值"（套件内的样本中位数），再取**轮间中位数**，然后
   `Δ% = (head − base) / base`。
4. 判定（默认 lower-better）：`|Δ| < noise_pct` → **持平**；变好 → **改善**；
   变差且 `|Δ| ≥ regress_pct` → **回归**；变差且介于两者之间 → **关注**。
   例外由 `thresholds.json` 声明：`higher_better`（越大越好）与 `info_only`（只给数值不判档）。
5. 输出：`ab.json`（机器）+ `comment.md`（人读，`report.py` 渲染）+ 每轮的 raw 结果。

**为什么交错**：同机负载是最大噪声源，同轮内 base/head 相邻测量能对冲慢漂移（turbo、
邻居 job、温度）。**为什么轮次中位数**：3 轮取中能丢掉单颗负载尖峰；2 轮的中位数实际是
两数均值，一颗尖峰就翻档（所以 CI 用 `--rounds 3`）。

**三层测量**，覆盖三类不同粒度的回归：

| 层 | 套件 | 测什么 | 不测什么 |
|---|---|---|---|
| 微基准 | `suite_rust.py`（criterion） | 纯 Rust 热路径（渲染、增量解析、图片编码、会话重放） | 协议、进程边界 |
| 服务层 | `suite_gateway.py`（真网关 + 假 Provider，HTTP/WS） | 流式扇出、轮次开销、大上下文装配、resume 重放 | TUI 渲染 |
| 端到端 | `suite_tui.py`（真网关 + 真 TUI + PTY） | 用户感知的显示延迟：provider 发射 → 标记出现在终端字节流 | 冷启动（窗口内含启动，但只有启动后的标记才计入） |

两侧共用 **head 的 harness**（`scripts/perf/`、`scripts/demo/` 与 `libs/wing-probe/`），
只有被测代码来自各自的 worktree——否则"改 rig"会同时改变两边的测量方式。代价：PR 同时改
产品与 rig 时，数字需要人工判读（见 §6）。

## 2. 四个套件与指标

契约（冻结，见任务书 §1–§4）：suite CLI 是
`<python> scripts/perf/suite_<name>.py --side <side.json> --out <out.json> --round <n> [--quick]`；
输出 JSON 是 `{suite, side, round, ok, error, metrics, samples, meta{duration_s,notes,config}}`；
指标 id 是 `<suite>.<case...>.<stat><unit>`，单位后缀 `_ns/_us/_ms/_ratio/_pct`。

### rust（criterion 微基准）

`--quick` = 每个 bench 一组锚定 case + `--measurement-time 2`（`tool_args_stream` 是
µs 级 bench，CI 实测轮间漂移 ~24%，单独给 3s；单侧 ≈ 2–3 分钟，支配项是
`tool_args_stream/frames_60fps/*`）；非 quick = 全 case、无 filter（含分钟级 case，
45 分钟级/侧，只作本地深潜、不做预算承诺）。指标 id 形如
`rust.stream_render.incremental.content.256.median_ns`（criterion id 的 `/` → `.`，
`metrics` 取 `estimates.json` 的 `median.point_estimate`）。

quick case 清单（契约 §4 的示例集合；共 16 项）：

| bench | quick cases |
|---|---|
| `stream_render` | `incremental/content/{64,256}` · `incremental/code_block/{64,256}` |
| `tool_args_stream` | `tool_args_stream/append/{256,512}` · `tool_args_stream/frames_60fps/{256,512}` |
| `image_frame` | `frame/pictures/4` · `scroll/4` · `first_encode/800x600` · `freshness/8` |
| `session_replay` | `replay/{1000,3000}` · `frame/{1000,3000}` |

一侧缺 bench target（如 base 早于本 PR 新增的 bench）→ 该 bench 的 case 记 **n/a**
（note 里写明原因），退出码仍 0，其余 bench 照常。取数的三层守卫：label
（`perf-<side>-r<round>-p<pid>`，含 pid 防并发/孤儿 writer 混入）→ mtime 新鲜度（只认本轮
cargo 启动之后写出的文件）→ quick 档的声明列表核对（多出来的 case 记 note 后丢弃）。

**质量信号会被聚合层消费**（不是只采集）：

- **载荷指纹**（`meta.config.payload_fingerprints`：bench 打印每个载荷大小的字节数与
  FNV-1a，`suite_rust` 收档）：**两侧都采到且不一致** → 该 suite 的指标判 `n/a`
  （"比的可能不是同一份载荷"——bench 载荷生成器被改过时 A/B 就失效了）；**仅单侧有**
  （新增 bench 的首 PR，base 侧没跑过）→ 跳过、不告警。
- **criterion 置信区间相对宽度**（`meta.config.ci_rel_pct`）：任一侧 > 10% → 聚合一条提示
  note（列最宽的几项），**不改判档**——宽度大是"样本抖动"，与"回归"是两件事。

### tui（端到端显示延迟）

假 Provider 按 3000 tok/s 灌语料（一帧一 token），每 400 帧插一行 `⟦M#####⟧` 标记；
`lag = 标记出现在 pty 字节流 − Provider 发射`。quick = 3000 tok/s × 6s × 1 次
（单侧 ≈ 8–10s）；默认档 = 3000×10s×3 + 30000×10s×1（单侧 ≈ 48s，30000 档**仅信息性**、
只进 `meta.config.informational`、不进指标）。

| metric | 含义 |
|---|---|
| `tui.display.lag_p50_us` / `lag_p99_us` / `lag_max_us` | 显示延迟分布（µs） |
| `tui.display.coverage_ratio` | 被采到的标记比例（**测量质量**，非性能；`info_only`） |
| `tui.display.trend_us` | 后 1/4 与前 1/4 的中位差（运行内漂移；零中心有符号量，`info_only`） |
| `tui.tui_cpu_ratio` | 送 prompt 到测量结束的 TUI 进程 CPU 占用比（CI runner 上 quick 档读到 0 → 该行 n/a） |

覆盖率的置信标注：`< 0.6` 时照常出指标 + `meta.notes` 记低置信。一次测量若**没有采到任何
可用的 lag 值**（TUI 没在窗口里画出首屏，或 provider 没发帧）最多尝试 2 次（= 1 次重跑，
rig 启动竞态而不是性能信号；两次都没有才算失败），重试与跳过都记 note。首击偶尔被 TUI 启动期
的终端查询冲掉——守卫是"provider 一帧都还没发"（emit log 为空）就补发同一句 prompt，
**首帧一出来即停止补发**（实测"网关开始路由 → provider 首帧"约 60ms，对 0.7s 阈值有 10x+ 余量）。

### gateway（socket 级）

四个场景各自独立（fresh `WING_HOME` + fresh 网关进程 + fresh 假 Provider），只用公开
HTTP / WS 驱动真网关。quick = 1000 帧扇出 / 12 轮 / 10×10KB 历史 / 2 次重启
（单侧 ≈ 4–6s）；full = 3000 / 30 / 30×10KB / 3。

| metric | 场景 | 窗口 |
|---|---|---|
| `gateway.fanout.complete_ms` | 1 prompt → 零延迟 1000 帧 | send → 末帧到达 |
| `gateway.fanout.gap_p99_us` | 同上 | 相邻帧到达间隔的 p99（客户端读循环） |
| `gateway.fanout.cpu_ms_per_1k` | 同上 | 网关进程 CPU 增量 / 千帧（darwin `proc_pid_rusage`、linux `/proc/<pid>/stat`） |
| `gateway.turn.p50_ms` / `p99_ms` | 12 个迷你轮次 | send → `turn_result`（p99 是 `info_only`：n=12，p99 = 最大值） |
| `gateway.context.build_p50_ms` / `build_p99_ms` | 大 history 后的 probe（quick 5 / full 8） | send → provider 收到请求体（同进程同钟；p99 是 `info_only`，n=5） |
| `gateway.resume.sync_ms` | 重启网关 → subscribe | subscribe 调用 → `sync_session` 到达（水合在窗口外，单列 `resume.hydrate_ms`） |

软异常（帧数对不上、请求数不是 1、重放条数与期望不符）一律写 `meta.notes`，不改退出码；
硬失败（超时、error 事件、指标缺失）→ `ok=false` + 退出码 3，并保留现场。

## 3. 本地怎么跑

```bash
# 离线自测（不构建、不联网、不建 worktree；覆盖聚合/判定/失败路径/CLI 端到端）
uv run python scripts/perf/ab.py --selftest

# 全量 quick（四个套件、两个侧、每侧两轮；默认 --prepare 会构建两侧）
uv run python scripts/perf/ab.py --repo . --base-ref origin/develop \
    --suites all --rounds 2 --quick

# 只跑一个套件 / 单侧
uv run python scripts/perf/ab.py --base-ref origin/develop --suites gateway --rounds 3 --quick
uv run python scripts/perf/suite_tui.py --side target/perf/sides/head.json \
    --out /tmp/tui.json --round 1 --quick

# 噪声地板：两侧同 rev（base 仍走独立的 worktree 与构建产物）
uv run python scripts/perf/ab.py --base-ref HEAD --calibrate --suites tui --rounds 3 --quick

# 只渲染 / 只贴评论
uv run python scripts/perf/report.py --json target/perf/ab.json --out-md /tmp/comment.md
uv run python scripts/perf/comment.py --repo owner/repo --pr 42 --body-file /tmp/comment.md --dry-run
```

产物落在 `<workdir>`（默认 `<repo>/target/perf`）：

```
<workdir>/
├── base/                       # git worktree（base 侧；跑完请清理）
├── sides/{base,head}.json      # 契约 §1 的 side 描述符
├── wing-home/{base,head}/      # 两侧隔离的 WING_HOME（套件自建自清）
├── raw/<suite>-<side>-r<n>.json   # 契约 §3 的单轮输出（含 suite 的 meta.notes）
├── raw/logs/                   # 每个子进程一份日志（suite 运行 + prepare，按命令分文件）
├── ab.json / comment.md        # 报告输入 / 渲染结果
```

清理：

```bash
git -C <repo> worktree remove --force target/perf/base && rm -rf target/perf
```

退出码：`0` = 全部测量成功；非 `0` = 有 `failures[]`（CI 视为基础设施故障）。suite 超时 /
构建失败 / 输出不合契约都进 `failures[]`，其余测量继续。`--prepare` 的 uv 命令是
`uv sync --frozen --no-install-package wing-cli`（跳过 maturin；本地若要 venv 里的 `wing`
脚本，自己跑一次 `uv sync --frozen`）。

## 4. 加东西的约定

**加一个套件**：写 `scripts/perf/suite_<name>.py`（遵守契约 §2/§3，`import common` 复用
`Side/SuiteOutput/run_command`）→ 在 `ab.py` 的 `SUITE_NAMES` 与 `SUITE_PREPARE` 注册
（后者是 `--prepare` 的构建命令，cwd = 该侧 worktree）→ 若要进 venv 检查，加进
`SUITE_NEEDS_VENV` → 指标 id 前缀必须是 suite 名（ab 会核对前缀并记 note）→
`thresholds.json` 补该族的带宽 → 在 `ab.py --selftest` 里加 stub 覆盖（离线可跑）。

**加一个指标**：id 用 `<suite>.<case...>.<stat><unit>`，`_ratio` 无量纲；默认
**lower-better**——"越大越好"必须写进 `thresholds.json.higher_better`，测量质量类指标
写进 `info_only`（只报数值不判档）；低置信/退化样本写 `meta.notes`（会带上
`<suite>/<side> r<round>` 汇总进报告），**绝不写 0 或 NaN**；先在本地量出噪声地板
（同一侧连跑 2–3 次 + `--calibrate`），带宽要有依据而不是拍脑袋。

**加一个 criterion bench / case**：bench 定义与载荷指纹照 `crates/wing/benches/` 的既有
惯例（`session_replay` 的载荷指纹可作模板）；在 `suite_rust.py` 的 `BENCHES` 里登记
quick 子集——quick 必须是该 bench 的**真子集**（全 case 仍能在非 quick 档跑）；
单侧 quick 预算的上限是 5 分钟（先量 `bench_seconds`）。

**改报告文字**：`report.py` 只做渲染（`merge_reports` 把多份 `ab.json` 并成一份，并在
sha / 轮数 / 档位不一致时插入告警、放弃单值表头）；改文案不碰 verdict 取值域
（`improved/flat/watch/regression/n/a`）。

## 5. 阈值与校准

`thresholds.json` 的形状（契约 §8，未知键会被忽略；缺 `info_only` 的旧文件照常加载）：

```json
{
  "default":  {"noise_pct": 6,  "regress_pct": 20},
  "overrides": {"<suite | metric 模式>": {"noise_pct": 15, "regress_pct": 30}},
  "higher_better": ["<metric 模式>"],
  "info_only": ["<metric 模式>"]
}
```

匹配规则：`overrides` 按**书写顺序**先命中先胜（`fnmatch`），所以更具体的模式必须写在
更宽的模式前面；loop 里没命中再回退到「suite 名」这一个键；最后回退 `default`。
`higher_better` / `info_only` 是独立的模式列表（与 `overrides` 无关）。

**校准怎么做**：`--calibrate` 让 base 侧也钉在 head rev（两套独立 worktree + 独立构建），
量到的就是"同一份代码的 A/B 流程噪声地板"。看 `ab.json` 的 `delta_pct` 分布：地板应当
集中在 0 附近（rust 实测 |Δ| ≤ 1.9%、中位 ≈ 0.2%）；某指标族地板明显高于当前带宽 → 把
`noise_pct` 抬到地板之上（宁可漏报也不要噪声伪装成结论）。CI 用 workflow_dispatch 的
`calibrate=true` 跑，产物在 artifact 里。

**CI 噪声地板实测**（校准 run `37672003292`：`calibrate=true`、两侧同 rev `e5d53e7`、
rounds=3、quick；注意它跑在**本轮改动之前**的口径上）：

- **gateway**：8 项里 6 项 |Δ| ≤ 2%（`context.build_p50` +0.4% / `build_p99` +0.6% /
  `fanout.cpu_ms_per_1k` ±0% / `resume.sync` −1.2% / `turn.p50` +1.7% / `turn.p99` −0.3%），
  `fanout.gap_p99` −5.5%（30% 带内）；唯一越界的 `fanout.complete_ms` **−57.4%** ——
  base 侧逐轮 295 / 297 / **127**ms（head 125–126 / 126 / 126ms）。
- **rust**：16 项里 **14 项 |Δ| ≤ 1.8%**（`stream_render` ≤ 0.18% / `frames_60fps` ≤ 0.42% /
  `session_replay` ≤ 0.56% / `image_frame` ≤ 1.78%）；两项越界：`tool_args_stream.append.256`
  **−14.9%**（base 逐轮 20.2 / 24.6 / 24.2 µs、head 20.6 / 19.8 / 20.9 µs → 轮间 spread
  21.8% vs 5.5%，与 run 1 的 23.8% 同量级）与 `append.512` **−2.5%**（放宽后的 12% 带内）——
  这正是给该 bench 放宽带宽（12/30）并单独加 `--measurement-time` 3s 的依据。
- **tui**：`lag_p50` +6.8%、`lag_p99` / `lag_max` −13.2%（都在 15/30、25/45 带内，判"持平"）；
  `trend` 在同一 rev 下给出 **−109.5%**（base 中位 +6.1ms vs head −0.6ms，逐轮值横跨
  ±8ms）—— 旧口径判"改善"，现在它是 `info_only`；coverage 两侧逐轮完全一致（0.7083 ×3）。

两条结论写进 CI 局限：**3 轮中位数挡得住单点尖峰、挡不住连续两轮被拖慢**，且**大 Δ 要先看
artifact 的逐轮值**。`append.*` 这一轮是 2s 测量时间的旧口径；下一次 calibrate dispatch 会带
3s 新口径重测，若同 rev 漂移仍 > 12% 就把带宽再放宽（本轮不预支）。


### 初始值与依据

| 指标族 | noise / regress | 依据 |
|---|---|---|
| 默认（rust 全族） | 6 / 20 | 06 的 `--calibrate`（两侧同 rev、1 轮）：12 项 \|Δ\| ≤ 1.9%、中位 ≈ 0.2%；同产品代码的 e2e 12 项 \|Δ\| ≤ 0.84%。6% 已是地板的 3 倍。 |
| `rust.tool_args_stream.append.*` | 12 / 30 | CI 首跑（run 37667281765）：base 侧 `append.256` 三轮 **24.1 / 29.5 / 29.8 µs**（轮间 spread 23.8%）、head 侧 `append.512` 60.1 / 55.3 / 59.0 µs（8.6%）——µs 级 bench 对 CPU 频率与邻居抖动特别敏感；同轮 ns 级 bench（`stream_render` content.64）只 0.3%。校准 run（同 rev）复现：`append.256` base 20.2 / 24.6 / 24.2 vs head 20.6 / 19.8 / 20.9 µs → 中位对比 −14.9%（当时是 2s 测量时间）。带宽放宽 + 该 bench 的 `--measurement-time` 单独给 3s（更多样本）；下一轮 calibrate 用新口径复测。 |
| `tui.display.lag_p50_us` | 15 / 30 | 04 的 `--calibrate`（quick、单轮）：Δp50 = +12.1% / −11.3% → 单轮地板 ≈ ±12%。CI 3 轮取中会更好，但带宽要站在"不误报"一侧。 |
| `tui.display.lag_p99_us` / `lag_max_us` | 25 / 45 | 04 实测 p99 常态 29–43ms、负载高时 60–115ms；quick 档一次只有约 20 个标记，p99/max 是小样本极值，比 p50 更跳。CI 首跑两侧逐轮 22.2/26.1/28.2ms 与 21.6/30.7/28.9ms 都在带内。 |
| `tui.tui_cpu_ratio` | 30 / 60 | CPU 采样是 `ps -o time=`（10ms 量化）对约 2s 窗口、约 3% 占用（≈60ms CPU）→ 量化不确定度 ≈17%。CI runner 上 quick 档两侧都读到 0（分辨率不够）→ 该行按"基准非正"判 n/a，不再显示成 0% 持平。 |
| `gateway.*`（其余 5 项：`fanout.complete_ms` / `fanout.cpu_ms_per_1k` / `turn.p50_ms` / `context.build_p50_ms` / `resume.sync_ms`） | 15 / 30 | 05 安静窗口（loadavg 4.2）同侧连跑 8 项，其中 **6 项 ≤ 8%**（complete 0.2% / cpu 0.8% / turn.p50 0.9% / turn.p99 7.6% / build_p50 1.2% / sync 0.7%）；高负载窗口（loadavg 9–10）可达 100%+。CI 首跑验证了 3 轮中位数的价值：`fanout.complete_ms` base r3 单点 156ms（其余 70/82ms）、`context.build_p50` head r2 单点 219ms（其余 ~20ms）都被中位数挡住。 |
| `gateway.fanout.gap_p99_us` | 30 / 50 | 1000 个帧间隔的 p99（不是小样本极值），但负载尖峰下仍可翻倍；05 的 `--calibrate --rounds 2` 在尖峰下出现过 −41%~+40% 的伪差异。CI 首跑 −6.6%。 |
| `tui.display.coverage_ratio` | — | **`info_only`**：覆盖率是测量质量，不判档（`verdict=n/a` + note；Δ 照算照显示）。 |
| `tui.display.trend_us` | — | **`info_only`**：零中心有符号量（后 1/4 中位 − 前 1/4 中位），`Δ% = (head−base)/base` 在 base<0 时符号翻转（wing-review 用仓库代码复现：base=−20ms→head=+20ms 判"改善"）；CI 首跑逐轮 −0.1/1.7/7.2ms，判档也没有信息量。 |
| `gateway.turn.p99_ms` / `gateway.context.build_p99_ms` | — | **`info_only`**：小样本（n=12 / n=5）下 p99 = 最大值。CI 首跑逐轮 turn.p99 base [73.0, 23.8, 76.8] vs head [90.6, 27.3, 23.9]、build.p99 base [36.1, 20.7, 51.6] vs head [176.7, 229.4, 52.4] → ±390%/±63% 全是调度噪声。对应的 p50 保留判档。 |

**`info_only` 的语义**：数值与 Δ 照算照显示、`verdict=n/a` + note「测量质量信息项（不参与
判定）」；进了 `info_only` 的指标不再单独给带宽（判档与带宽无关），将来若要恢复判档，按
上面的校准流程重新量地板。当前名单：`tui.display.coverage_ratio`、`tui.display.trend_us`、
`gateway.turn.p99_ms`、`gateway.context.build_p99_ms`。

**负/零基准守卫**：`base_median <= 0` 的指标一律 `n/a` + note「基准非正…Δ% 无定义」——
百分比在非正基准上没有方向含义（有符号量会翻转、零基准无定义）。

这些是**初值**：CI 落地的 `--calibrate` 数据到手后按上面的流程收窄/加宽——**带宽表与
`ab.py` selftest 里的钉值表要一起改**（两处：`thresholds.json` 与 `run_selftest()` 的
`thresholds.file …` 循环；selftest 逐点核对各族的带宽与匹配顺序，只改一处会立刻红灯——
这是刻意的：把带宽的静默漂移变成显式改动）。

## 6. CI 行为与局限

- **触发**：`pull_request`（opened / synchronize / reopened / ready_for_review，目标 `develop`）
  与手工 `workflow_dispatch`（`pr_number` 必填、`calibrate` 可选）。草稿**只在 pull_request
  路径**跳过（dispatch 由人显式指定、想测草稿就能测）；同 PR 的后续 run 会取消上一轮（`concurrency`）。
- **两个测量 job**（`perf-tui` = rust + tui，`perf-gateway` = gateway）+ 一个 `perf-report`
  汇总 job；`--quick --rounds 3`，产物上传为 `perf-tui` / `perf-gateway` / `perf-comment`
  artifact（保留 14 天），评论正文同时进 job summary。
- **构建**：uv 一律 `uv sync --frozen --no-install-package wing-cli`（跳过 maturin 编译——
  套件只用 `wing-gateway` 与各侧 `target/release/wing`，不需要 venv 里的 `wing` 脚本），
  之后必须 `uv run --no-sync`（否则 `uv run` 自己再 sync 一次、把 wing-cli 装回来）；
  `perf-gateway` 不跑任何 cargo 命令，因此**不再安装 Rust toolchain / rust-cache**。
  `perf-tui` 保留 Rust。
  *follow-up*：base 侧 worktree 建在 `$RUNNER_TEMP/perf/base`（workspace 之外），
  `rust-cache` 默认只覆盖 workspace 下的 `target/`，所以 base 侧每轮都冷编译——
  给缓存加 `cache-directories: ${{ runner.temp }}/perf/base/target` 是下一步的优化（本轮不做，
  避免同时引入"缓存命中/未命中"两种构建状态）。
- **评论**：`report.py` 渲染 → `comment.py` 按 marker `<!-- wing-perf -->` upsert（同 PR 永远
  只有一条）。**fork PR 不发评论**（token 只读），结果只留 artifact。
- **job 不够红**：性能回归只会出现在评论里；红叉只留给基础设施故障（构建/测量失败、渲染失败）。
- **已知局限**：
  - harness 恒取 head 版——PR 同时改 `scripts/perf/`、`scripts/demo/` 或 `libs/wing-probe/`
    时，两边的测量方式可能不同，需要人工判读（这也是 `docs/dev/*` 之外唯一"两侧不对等"的地方）。
  - 新增 bench / 指标的首个 PR：base 侧没有该 bench → 对应行是 `n/a`（不是失败）。
  - runner 噪声与 vCPU 抖动不可避免；`--quick` 档的样本量小，p99 型指标天然离散。µs 级
    bench（`tool_args_stream.append.*`）的**轮间**漂移 CI 实测可达 ~24%（同一轮的样本内
    波动很小）——带宽已放宽，`--measurement-time` 也单独加到 3s。
  - `tui.tui_cpu_ratio` 在 CI runner 上 quick 档两侧都读到 0（`ps -o time=` 分辨率不够）
    → 该行按"基准非正"判 n/a；本地开发机可见非零值。
  - **大 Δ 先看逐轮值**：校准 run（两侧同 rev）实测 `gateway.fanout.complete_ms` −57%、
    `rust.tool_args_stream.append.256` −14.9%、`tui.display.trend_us` −109% —— 轮间中位数
    只挡单点尖峰。artifact 的 `raw/<suite>-<side>-r<n>.json` 才是判读的第一现场。
  - gateway 套件失败时现场留在 runner 的 `<RUNNER_TEMP>/perf/wing-home/` 里，artifact 只带
    `raw/`（JSON + 日志）——要看完整现场需要 runner 还活着（或本地复跑）。
  - 不跑 nightly 全量档（非 quick）、不维护跨 run 趋势、不做强制门禁、不测内存/二进制体积/
    构建时间/冷启动/真实 LLM 时延。

## 7. 排查手册

| 症状 | 先看哪里 |
|---|---|
| run 非 0 退出 / 评论里"n 个套件失败" | `[perf] FAIL <suite>/<side> r<n>` 那几行（stdout）；两段 `log`/`log_tail` 在 `ab.json` 的 `failures[]` 里 |
| 某侧 suite 失败 | `<workdir>/raw/logs/<suite>-<side>-r<n>.log`（套件 stdout+stderr）；CI 上在 artifact 的 `raw/logs/` |
| 构建失败 | `<workdir>/raw/logs/prepare-<suite>-<side>-<i>-<cmd>.log` |
| rust 行大面积 `n/a` | 该侧缺 bench（`cases` 列表 / notes 写着 `no bench target named …`）——正常现象；否则查 `criterion_dir`、`CARGO_TARGET_DIR` / `CRITERION_HOME` 是否被外部设置（**相对值**会把原因写成一条 note：套件相对 worktree 解析、criterion 相对 bench cwd） |
| tui 行低置信 / 行缺失 | `meta.config.runs[]` 的 `coverage` / `markers` / `missing` / `resends`；`meta.notes` 会点出"低置信""重试""补发" |
| rust 整片 `n/a` 且 notes 说"载荷指纹不一致" | 两侧 bench 的载荷不是同一份（改了 `session_replay` 的载荷生成器）→ 数字不可比；确认载荷本该一致还是改动者有意改载荷 |
| 评论里出现"criterion 置信区间偏宽" | 对应 raw 的 `meta.config.ci_rel_pct`（样本抖动，不改判档）；必要时重跑或加大 `--measurement-time` |
| 某行 note 是"基准非正" | 基准侧中位数 ≤ 0（如 `tui.tui_cpu_ratio` 在 CI runner 上读到 0）——本次 run 里该指标没有百分比含义 |
| gateway 数字整体偏大 | `meta.config.machine.loadavg`（判读同机负载的第一手证据）；再用 `--calibrate` 量地板 |
| 评论没出现 | 是不是 fork PR / draft / 上一轮 run 被取消；`perf-report` job 的 `compose` 步骤输出；artifact `perf-comment` |
| 想复现某一行 | 用 `ab.json` 里该 suite 的 `meta.config`（档位、并发、历史规模）在本地单侧跑 `suite_<name>.py`；side 描述符形状见 `scripts/perf/README.md` |

## 8. 与其它设施的关系

- `scripts/demo/latency.py`：面向人的"显示延迟水位尺"（表格式输出），tui 套件复用它
  `--json` 之外的核心测量函数；`make demo` 的录制路径不受影响。
- `crates/wing/benches/`：criterion 基准（`stream_render` / `tool_args_stream` / `image_frame` /
  `session_replay`），rust 套件是它们的 CI 子集 + 收数器。
- `libs/wing-probe/`：gateway / tui 套件的驱动层（真网关 + 假 Provider）；套件不改产品代码。
- `.github/workflows/wing-review.yml`：AI 审查者会读取带 marker 的性能评论，把与改动相关的
  回归点并入结论（作为证据之一）。
