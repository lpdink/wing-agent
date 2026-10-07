# scripts/perf — PR 级性能回归测量（速查）

以 merge-base 为对照、同 run 内交错 A/B 跑四个套件（rust criterion / 端到端 TUI /
socket 级 gateway / 汇总报告），产物是 PR 上的一条 sticky comment。方法、指标表、
阈值依据与排查手册见 **[docs/dev/perf-testing.md](../../docs/dev/perf-testing.md)**；
本页只是命令与文件形状。

## 常用命令

```bash
# 离线自测（不构建、不联网、不建 worktree；改 harness 后先跑它）
uv run python scripts/perf/ab.py --selftest

# 全量 quick（默认 --prepare：两侧 cargo / uv 构建；默认 rounds=2）
uv run python scripts/perf/ab.py --base-ref origin/develop --suites all --rounds 2 --quick

# 单个套件 / 指定轮数（CI 是 --rounds 3）
uv run python scripts/perf/ab.py --base-ref origin/develop --suites gateway --rounds 3 --quick

# 噪声地板：两侧同 rev（CI 的 dispatch 输入 calibrate=true 等价）
uv run python scripts/perf/ab.py --base-ref HEAD --calibrate --suites tui --rounds 3 --quick

# 单侧 suite（调试用；--round 是契约必填）
uv run python scripts/perf/suite_rust.py --side target/perf/sides/head.json \
    --out /tmp/rust.json --round 1 --quick

# 只渲染评论 / 只演练贴评论（dry-run 不联网）
uv run python scripts/perf/report.py --json target/perf/ab.json --out-md /tmp/comment.md
uv run python scripts/perf/comment.py --repo owner/repo --pr 42 --body-file /tmp/comment.md --dry-run
```

`ab.py` 参数（`--help` 为准）：`--repo`（head worktree，默认脚本所在仓库）、
`--base-ref`（必填，除 `--selftest`）、`--workdir`（默认 `<repo>/target/perf`）、
`--suites`（`all` 或 `rust,tui,gateway`）、`--rounds`、`--quick`、`--prepare/--no-prepare`、
`--calibrate`、`--out-json` / `--out-md`、`--selftest`。

`--quick` = CI 档（单侧单轮有界：rust ≈ 2–3 min、tui ≈ 8–10s、gateway ≈ 4–6s）；
不带 `--quick` 是本地深潜档（rust 全 case 45 分钟级/侧、tui ≈ 48s/侧）。

`--prepare`（默认）按套件构建两侧：rust 用 `cargo bench --no-run -p wing`；tui 用
`cargo build --release -p wing` + uv；gateway 只需 uv。uv 一律
`uv sync --frozen --no-install-package wing-cli`（跳过 maturin 编译——套件只用
`wing-gateway` 与 `target/release/wing`）；本地若要 venv 里的 `wing` 脚本，自己跑一次
`uv sync --frozen`。

## side.json（契约 §1）

`ab.py` 自己写 `<workdir>/sides/{base,head}.json`；手搓单侧时照抄这个形状（全绝对路径）：

```json
{
  "name": "head",
  "worktree": "/abs/path/head-worktree",
  "wing_bin": "/abs/path/head-worktree/target/release/wing",
  "gateway_bin": "/abs/path/head-worktree/.venv/bin/wing-gateway",
  "python": "/abs/path/head-worktree/.venv/bin/python",
  "wing_home": "/abs/scratch/wing-home/head"
}
```

`wing_home` 必须是绝对路径（gateway 套件会拒绝相对路径）且归套件独占使用。

## 产物与清理

```
<workdir>/base/                    # base worktree
<workdir>/sides/{base,head}.json   # side 描述符
<workdir>/wing-home/{base,head}/   # 隔离的 WING_HOME（成功即删；失败保留现场）
<workdir>/raw/<suite>-<side>-r<n>.json   # 契约 §3 单轮输出（含 suite 的 meta.notes）
<workdir>/raw/logs/…                      # 子进程日志（suite 按 run，prepare 按命令）
<workdir>/ab.json  +  <workdir>/comment.md
```

```bash
git -C <repo> worktree remove --force target/perf/base && rm -rf target/perf
```

退出码：`0` = 全部测量成功（含"部分指标 n/a"）；非 `0` = `failures[]` 非空
（CI 视为基础设施故障）。suite 自己的退出码：`rust` 1 = 数据不可信、缺 bench 仍是 0；
`tui` 1 = 测量失败、2 = 参数/环境错误；`gateway` 2 = 参数/环境错误、3 = 场景失败。

## thresholds.json

```json
{
  "default":  {"noise_pct": 6,  "regress_pct": 20},
  "overrides": {"<suite 名或 metric 模式>": {"noise_pct": 15, "regress_pct": 30}},
  "higher_better": ["<metric 模式>"],
  "info_only": ["<metric 模式>"]
}
```

- `overrides` 按**书写顺序**先命中先胜（`fnmatch`）——具体的模式写在宽的之前；没命中再
  回退同名 suite 的键，最后 `default`。各族的当前值与依据见
  [docs/dev/perf-testing.md §5](../../docs/dev/perf-testing.md)。
- `higher_better`：越大越好的指标；`info_only`：只给数值、不判档（`verdict=n/a` + note）。
  当前名单：`tui.display.coverage_ratio`、`tui.display.trend_us`、`gateway.turn.p99_ms`、
  `gateway.context.build_p99_ms`。
- 另有一条通用守卫：`base_median <= 0` 的指标一律 n/a（Δ% 在非正基准上没有方向含义）。
- 改带宽要**同时改** `ab.py` selftest 里的钉值表（`thresholds.file …` 循环），否则红灯。
