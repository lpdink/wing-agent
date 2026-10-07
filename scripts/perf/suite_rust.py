#!/usr/bin/env python3
"""Rust criterion 收集套件：把一侧 worktree 的 criterion 中位数转成契约指标。

    <python> scripts/perf/suite_rust.py --side <side.json> --out <out.json> --round <n> [--quick] [--benches a,b]

- quick 档 = CI 子集（每个 bench 一组锚定过滤正则 + `--measurement-time 2`，
  `tool_args_stream` 3s——µs 级 bench 的轮间漂移实测 ~24%，见 design D8），单侧 ≈ 2–3 分钟；
  非 quick 档 = 每个 bench 的全部 case（不加 filter，`--measurement-time 3`）——含 `baseline/*/512`
  这类分钟级 case，本机 45 分钟级/侧，只作本地深潜用、不做预算承诺（`--benches` 可只跑其中几个）。
- 调档只用 `<filter>` 与 `--measurement-time`：各 bench 在 group 里钉了 `sample_size(10)` /
  `warm_up_time(1s)`，`--sample-size` / `--warm-up-time` 调了等于没调（且 `--sample-size < 10`
  会被 criterion 断言打崩，exit 101）。
- 取数：`target/criterion/**/<label>/estimates.json` 的 `median.point_estimate`（纳秒），
  label = `perf-<side>-r<round>-p<pid>`（含进程 pid：同侧同轮的并发/孤儿 writer 不会混进来）。
  criterion id 取同目录 `benchmark.json` 的 `full_id` —— 目录名会把 `/` 换成 `_`
  （`frame/pictures/4` 的目录是 `frame/pictures_4/`），不能从路径反推；`benchmark.json`
  缺失（残局）时退化为目录相对路径并去掉最后一段 label。
  criterion 根目录镜像 criterion 0.5.1 的解析顺序：`$CRITERION_HOME` →
  `$CARGO_TARGET_DIR/criterion` → `<worktree>/target/criterion`（相对值的基准差异见
  `criterion_root`：套件相对 worktree、criterion 相对 bench cwd；命中时会写一条 note）。
- 一侧缺 bench target（本 PR 的 base 早于 session_replay）→ 该 bench 的 case 记 n/a（notes），
  **退出码 0**，其余 bench 照常；编译错误 / 超时 / 坏 JSON → `ok=false` + 退出码 1
  （ab.py 记 infrastructure failure）。

设计与取舍（过滤器表、n/a 语义、预算实测）见 perf-ci 06 步骤的 `design.md`。
"""

from __future__ import annotations

import argparse
import math
import os
import re
import sys
import tempfile
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from common import (  # noqa: E402  (同目录模块：脚本直接跑时 sys.path 已含 HERE)
    CommandError,
    Side,
    SuiteOutput,
    read_json,
    run_command,
    strip_ansi,
)

#: suite 名（= metric id 的第一段，契约 §4）。
SUITE = "rust"

#: quick（CI）档的 `--measurement-time`（秒）与每 bench 超时（秒，给冷编译留余量）。
QUICK_MEASUREMENT_S = 2
FULL_MEASUREMENT_S = 3
QUICK_TIMEOUT_S = 900
FULL_TIMEOUT_S = 3600

#: 单个 bench 的 quick 测量时长覆盖（秒）：`tool_args_stream` 是 µs 级 bench，CI 上轮间
#: 漂移实测 ~24%（run 37667281765 的 `append.256`：24.1 / 29.5 / 29.8 µs），多给 1s
#: 换更稳的中位数；只加这一个 bench，别让整档变慢。
QUICK_MEASUREMENT_S_OVERRIDES: Mapping[str, int] = {"tool_args_stream": 3}

#: cargo 在目标不存在时的报错 —— 这是"一侧缺 bench"的正常情形，不是失败。
_MISSING_TARGET = re.compile(r"no bench target named")

#: session_replay 的载荷指纹行（bench 在测量前打到 stderr，两侧字节数/哈希应一致）。
_PAYLOAD_FINGERPRINT = re.compile(
    r"session_replay: payload n=(\d+) seed (0x[0-9a-fA-F]+): (\d+) bytes, fnv1a ([0-9a-f]{16})"
)


@dataclass(frozen=True)
class Bench:
    """一个 cargo bench 目标，以及 quick 档要收集的 case（criterion id）。"""

    name: str
    quick_cases: tuple[str, ...]


#: 四个 bench 的 quick 子集（契约 §4 的示例集合；完整 case 清单见 02 的 design.md）。
#: 生成的过滤正则是锚定精确式（见 filter_for），覆盖与任务书 filter 相同的集合。
BENCHES: tuple[Bench, ...] = (
    Bench(
        "stream_render",
        (
            "incremental/content/64",
            "incremental/content/256",
            "incremental/code_block/64",
            "incremental/code_block/256",
        ),
    ),
    Bench(
        "tool_args_stream",
        (
            "tool_args_stream/append/256",
            "tool_args_stream/append/512",
            "tool_args_stream/frames_60fps/256",
            "tool_args_stream/frames_60fps/512",
        ),
    ),
    Bench(
        "image_frame",
        (
            "frame/pictures/4",
            "scroll/4",
            "first_encode/800x600",
            "freshness/8",
        ),
    ),
    Bench(
        "session_replay",
        (
            "replay/1000",
            "replay/3000",
            "frame/1000",
            "frame/3000",
        ),
    ),
)


class SuiteError(RuntimeError):
    """本侧数据不可信（编译错误、超时、坏 JSON、缺 worktree）——ok=false + 退出码 1。"""


@dataclass(frozen=True)
class Estimate:
    """一个 case 的 criterion 结果：中位数（ns）与 95% 置信区间相对宽度。"""

    criterion_id: str
    median_ns: float
    ci_rel_pct: float | None
    id_from_dir: bool


@dataclass(frozen=True)
class BenchRun:
    """一次 cargo bench 调用的结果。"""

    rc: int
    seconds: float
    missing: bool
    output: str

    @property
    def tail(self) -> str:
        return self.output[-1200:].strip()

    @property
    def first_line(self) -> str:
        """输出里第一行有内容的东西（首行是 run_command 写的命令回显，跳过）。

        cargo 的彩色 stderr 带 ANSI 码（CI 上尤其明显），这条线会进 `meta.notes` 与
        失败摘要、最终出现在 PR 评论里 —— 先剥掉。
        """
        for line in self.output.splitlines():
            stripped = strip_ansi(line).strip()
            if stripped and not stripped.startswith("$ "):
                return stripped
        return "(no output)"


# ── 命名与路径 ────────────────────────────────────────────────


def filter_for(cases: Sequence[str]) -> str:
    """声明 case 列表 → 锚定的过滤正则（criterion 的 FILTER 是正则、子串匹配）。"""
    return "^(" + "|".join(re.escape(case) for case in cases) + ")$"


def metric_id(bench: str, criterion_id: str) -> str:
    """契约 §4：`rust.<bench>.<criterion id 的 / → .>.median_ns`。

    criterion id 的前导段与 bench 同名时去掉：`tool_args_stream` 的 group 与 target 同名，
    原样映射会得到 `rust.tool_args_stream.tool_args_stream.append.512.median_ns`，
    而契约 §4 的示例是 `rust.tool_args_stream.append.512.median_ns`（同形的还有
    `rust.session_replay.replay.1000.median_ns`）。原始 criterion id 完整保留在
    `meta.config.cases` 里，映射可审计。
    """
    parts = [part for part in criterion_id.split("/") if part]
    if len(parts) > 1 and parts[0] == bench:
        parts = parts[1:]
    return f"{SUITE}.{bench}.{'.'.join(parts)}.median_ns"


def safe_label(side_name: str, round_no: int) -> str:
    """`--save-baseline` 的 label：`perf-<side>-r<round>-p<pid>`。

    side/round 让两侧、各轮互不覆盖；pid 让**同侧同轮的并发 / 孤儿 writer**也不撞名——
    收集是"按 label 精确 glob + mtime 新鲜度"两步，label 相同的话，一个被杀掉父进程后
    仍在写盘的 criterion 二进制会把它的 case 混进下一个进程的收集窗口（实测发生过）。
    """
    safe = re.sub(r"[^A-Za-z0-9._-]", "_", side_name) or "side"
    return f"perf-{safe}-r{round_no}-p{os.getpid()}"


def measurement_seconds(bench: str, *, quick: bool) -> int:
    """该 bench 的 `--measurement-time`（秒）：quick 档允许按 bench 覆盖（见常量注释）。"""
    if not quick:
        return FULL_MEASUREMENT_S
    return QUICK_MEASUREMENT_S_OVERRIDES.get(bench, QUICK_MEASUREMENT_S)


def select_benches(raw: str) -> tuple[Bench, ...]:
    """`--benches` 选择器（本地调试用）：名字未知立刻报错，不静默跑一半。

    选择器只做过滤——执行顺序恒为 `BENCHES` 的规范顺序（与不加 flag 时一致）。
    """
    names = [part.strip() for part in raw.split(",") if part.strip()]
    if not names:
        raise SuiteError("--benches: no bench selected")
    by_name = {bench.name: bench for bench in BENCHES}
    unknown = [name for name in names if name not in by_name]
    if unknown:
        raise SuiteError(
            f"--benches: unknown bench(es) {', '.join(unknown)}; "
            f"known: {', '.join(by_name)}"
        )
    selected = set(names)
    return tuple(bench for bench in BENCHES if bench.name in selected)


def criterion_root(worktree: Path) -> Path:
    """criterion 的输出目录。

    镜像 criterion 0.5.1 的解析顺序（`criterion-0.5.1/src/lib.rs:134-144`）：
    ``$CRITERION_HOME`` → ``$CARGO_TARGET_DIR/criterion`` → ``./target/criterion``
    （第三档是 cargo metadata 的 target dir，非 workspace 场景这里不做等价实现）。

    基准差异（只影响**相对值**这种非常规配置）：本套件对相对路径按 cargo 的规则相对
    ``worktree`` 解析，而 criterion 对 ``CRITERION_HOME`` / ``CARGO_TARGET_DIR`` 的
    相对值不做归一化、直接相对 bench 进程的 cwd（= 包根，如
    ``<worktree>/crates/wing``）解析；两者不一致时套件读不到数据（全 case n/a、退出码
    仍 0，`relative_override_note()` 会把原因写进 ``meta.notes``）。绝对路径（CI 与常规
    用法）不受影响。
    """
    for key, suffix in (("CRITERION_HOME", ""), ("CARGO_TARGET_DIR", "criterion")):
        override = os.environ.get(key)
        if override:
            path = Path(override)
            base = path if path.is_absolute() else worktree / path
            return (base / suffix).resolve() if suffix else base.resolve()
    return worktree / "target" / "criterion"


def relative_override_note(worktree: Path) -> str | None:
    """相对 `CRITERION_HOME` / `CARGO_TARGET_DIR` 的基准差异提示（没有则 None）。

    环境里有人设了相对值时才产出：这类配置下"套件读的位置"与"criterion 写的位置"
    可能不是同一处，现场只会表现为"全 case n/a"——把原因写成一条 note，别让排查靠猜。
    """
    for key in ("CRITERION_HOME", "CARGO_TARGET_DIR"):
        value = os.environ.get(key)
        if value and not Path(value).is_absolute():
            return (
                f"{key}={value!r} 是相对路径：本套件相对 worktree 解析，criterion 自身相对 "
                f"bench cwd（包根，如 {worktree / 'crates' / 'wing'}）解析——两者可能不是"
                "同一处，读不到数据时请改用绝对路径"
            )
    return None


# ── 取数 ─────────────────────────────────────────────────────


def find_estimates(root: Path, label: str, since: float) -> list[Estimate]:
    """glob `<label>/estimates.json`（只认 mtime >= since 的本轮文件），读中位数。

    `since` 是本次 cargo 调用开始时刻：同一个 criterion root 里还留着更早 bench / 更早轮次
    的数据，label 区分不了"谁写的"，mtime 是唯一可用的每文件时间戳。
    """
    found: list[Estimate] = []
    if not root.is_dir():
        return found
    for path in sorted(root.glob(f"**/{label}/estimates.json")):
        try:
            stamp = path.stat().st_mtime
        except OSError:  # pragma: no cover - 文件在 glob 与 stat 之间消失
            continue
        if stamp < since:
            continue
        found.append(_load_estimate(path, root))
    return found


def _load_estimate(path: Path, root: Path) -> Estimate:
    """读一个 `estimates.json` → Estimate；结构不对 → SuiteError（数据不可信，不猜）。"""
    try:
        payload = read_json(path)
    except (ValueError, CommandError) as exc:
        raise SuiteError(f"cannot read {path}: {exc}") from exc
    median = payload.get("median") if isinstance(payload, Mapping) else None
    point = median.get("point_estimate") if isinstance(median, Mapping) else None
    if not isinstance(point, (int, float)) or isinstance(point, bool):
        raise SuiteError(f"{path}: no numeric median.point_estimate")

    report_dir = path.parent
    return Estimate(
        criterion_id=_criterion_id(report_dir, root),
        median_ns=float(point),
        ci_rel_pct=_ci_rel_pct(median, float(point)),
        id_from_dir=not (report_dir / "benchmark.json").is_file(),
    )


def _criterion_id(report_dir: Path, root: Path) -> str:
    """criterion id：`benchmark.json` 的 `full_id`（事实来源）；缺失 → 目录名兜底。"""
    try:
        payload = read_json(report_dir / "benchmark.json")
    except (ValueError, CommandError):
        payload = None
    if isinstance(payload, Mapping):
        full_id = payload.get("full_id")
        if isinstance(full_id, str) and full_id:
            return full_id
    try:
        parts = report_dir.relative_to(root).parts
    except ValueError:  # pragma: no cover - glob 结果必然在 root 下
        parts = (report_dir.name,)
    # 最后一段是 `--save-baseline` 的 label（`perf-<side>-r<round>-p<pid>`），不是
    # criterion id 的一部分：留着会让 quick 档把它误判成 filter drift、full 档写出
    # 逐轮不同的 metric id。
    return "/".join(parts[:-1] if len(parts) > 1 else parts)


def _ci_rel_pct(median: Mapping[str, Any], point: float) -> float | None:
    """95% 置信区间相对宽度（%）——低置信可见；结构不符就如实不给。"""
    interval = median.get("confidence_interval")
    if not isinstance(interval, Mapping) or point <= 0:
        return None
    lower = interval.get("lower_bound")
    upper = interval.get("upper_bound")
    if not isinstance(lower, (int, float)) or not isinstance(upper, (int, float)):
        return None
    return round(100.0 * (float(upper) - float(lower)) / point, 3)


def parse_payload_fingerprints(output: str) -> dict[str, dict[str, Any]]:
    """session_replay 的载荷指纹（bytes + FNV-1a）：两侧不一致 = 比的不是同一份载荷。"""
    found: dict[str, dict[str, Any]] = {}
    for match in _PAYLOAD_FINGERPRINT.finditer(output):
        size, seed, byte_count, digest = match.groups()
        found[size] = {"seed": seed, "bytes": int(byte_count), "fnv1a": digest}
    return found


# ── 调用 cargo ────────────────────────────────────────────────


def run_bench(
    worktree: Path,
    bench: str,
    *,
    filt: str | None,
    measurement_s: int,
    timeout_s: int,
    label: str,
    log_dir: Path,
) -> BenchRun:
    """`cargo bench -p wing --bench <name> -- [<filter>] --measurement-time N --save-baseline <label>`。"""
    cmd = ["cargo", "bench", "-p", "wing", "--bench", bench, "--"]
    if filt is not None:
        cmd.append(filt)
    cmd += ["--measurement-time", str(measurement_s), "--save-baseline", label]
    log_path = log_dir / f"{bench}.log"
    try:
        result = run_command(
            cmd, cwd=worktree, log_path=log_path, timeout=float(timeout_s), echo=True
        )
    except CommandError as exc:
        raise SuiteError(f"cannot run cargo for bench {bench}: {exc}") from exc
    output = ""
    if log_path.is_file():
        output = log_path.read_text(encoding="utf-8", errors="replace")
    return BenchRun(
        rc=result.rc,
        seconds=result.seconds,
        missing=bool(_MISSING_TARGET.search(output)),
        output=output,
    )


def rustc_version(worktree: Path) -> str:
    """工具链 provenance（两侧不同即"换了编译器"的对比，读不到就如实写 unknown）。"""
    try:
        result = run_command(["rustc", "--version"], cwd=worktree)
    except CommandError:
        return "unknown"
    return result.tail.strip() or "unknown"


# ── 收集主流程 ────────────────────────────────────────────────


def collect(
    side: Side, out: SuiteOutput, *, quick: bool, benches: Sequence[Bench] = BENCHES
) -> None:
    """按 bench 顺序收集；缺 bench → n/a note，其它错误 → SuiteError。"""
    worktree = side.worktree
    if not worktree.is_dir():
        raise SuiteError(f"side {side.name}: worktree missing: {worktree}")
    root = criterion_root(worktree)
    label = safe_label(side.name, out.round)
    relative_note = relative_override_note(worktree)
    if relative_note:
        # 放在最前：它解释的是"为什么下面全是 n/a"，比逐 case 的 n/a note 更接近原因。
        out.note(relative_note)

    filters: dict[str, str | None] = {}
    measurement: dict[str, int] = {}
    durations: dict[str, float] = {}
    cases: dict[str, list[str]] = {}
    ci: dict[str, float] = {}
    fingerprints: dict[str, dict[str, Any]] = {}

    for bench in benches:
        filt = filter_for(bench.quick_cases) if quick else None
        seconds = measurement_seconds(bench.name, quick=quick)
        timeout = QUICK_TIMEOUT_S if quick else FULL_TIMEOUT_S
        filters[bench.name] = filt
        measurement[bench.name] = seconds

        started = time.time()
        with tempfile.TemporaryDirectory(prefix="wing-perf-rust-") as scratch:
            run = run_bench(
                worktree,
                bench.name,
                filt=filt,
                measurement_s=seconds,
                timeout_s=timeout,
                label=label,
                log_dir=Path(scratch),
            )
        durations[bench.name] = round(run.seconds, 3)
        fingerprints.update(parse_payload_fingerprints(run.output))

        if run.missing:
            cases[bench.name] = []
            out.note(
                f"{bench.name}: bench target missing on side {side.name} "
                f"({run.first_line}) — cases n/a"
            )
            print(
                f"[rust] {bench.name}: bench target missing on this side — cases n/a",
                flush=True,
            )
            continue
        if run.rc != 0:
            print(run.tail, file=sys.stderr, flush=True)
            raise SuiteError(
                f"cargo bench --bench {bench.name} exited {run.rc} after {run.seconds}s: "
                f"{run.first_line}"
            )

        emitted = _emit_estimates(
            out, bench, find_estimates(root, label, started), quick=quick, ci=ci
        )
        cases[bench.name] = emitted
        print(
            f"[rust] {bench.name}: rc=0 in {run.seconds}s, {len(emitted)} cases "
            f"(filter={filt or 'all'})",
            flush=True,
        )

    out.setting("baseline_label", label)
    out.setting("benches", [bench.name for bench in benches])
    out.setting("criterion_dir", str(root))
    out.setting("filters", filters)
    out.setting("measurement_time_s", measurement)
    out.setting("bench_seconds", durations)
    out.setting("cases", cases)
    if ci:
        out.setting("ci_rel_pct", ci)
    if fingerprints:
        out.setting("payload_fingerprints", fingerprints)
    out.setting("rustc_version", rustc_version(worktree))


def _emit_estimates(
    out: SuiteOutput,
    bench: Bench,
    estimates: Sequence[Estimate],
    *,
    quick: bool,
    ci: dict[str, float],
) -> list[str]:
    """把发现的本轮 estimate 写进输出；quick 档按声明列表核对（缺 case → n/a note）。"""
    produced = {estimate.criterion_id: estimate for estimate in estimates}
    emitted: list[str] = []
    if quick:
        for case in bench.quick_cases:
            estimate = produced.pop(case, None)
            if estimate is None:
                out.note(
                    f"{bench.name}: no estimate for {case} — n/a "
                    f"(filter matched nothing / case renamed?)"
                )
                continue
            _emit(out, bench.name, estimate, ci)
            emitted.append(case)
        for leftover in sorted(produced):
            out.note(
                f"{bench.name}: unexpected case {leftover} (filter drift?) — ignored"
            )
    else:
        for case in sorted(produced):
            _emit(out, bench.name, produced[case], ci)
            emitted.append(case)
    return emitted


def _emit(
    out: SuiteOutput, bench: str, estimate: Estimate, ci: dict[str, float]
) -> None:
    """一条 estimate → 一个 metric；不可用的值记 note 而不是写 0。"""
    metric = metric_id(bench, estimate.criterion_id)
    if not math.isfinite(estimate.median_ns) or estimate.median_ns <= 0:
        out.note(
            f"{bench}: estimate for {estimate.criterion_id} not usable "
            f"({estimate.median_ns!r}) — n/a"
        )
        return
    if estimate.id_from_dir:
        out.note(
            f"{bench}: {estimate.criterion_id} has no benchmark.json — id from directory"
        )
    out.metric(metric, estimate.median_ns)
    if estimate.ci_rel_pct is not None:
        ci[metric] = estimate.ci_rel_pct


# ── CLI ───────────────────────────────────────────────────────


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="suite_rust.py",
        description=(
            "perf-ci rust suite: collect criterion medians from one side into the "
            "contract suite output (see scripts/perf/README.md and the 06 design)"
        ),
        epilog=(
            "example: uv run python scripts/perf/suite_rust.py "
            "--side target/perf/sides/head.json --out /tmp/rust.json --round 1 --quick"
        ),
    )
    parser.add_argument(
        "--side", required=True, help="side descriptor JSON (contract §1)"
    )
    parser.add_argument(
        "--out", required=True, help="suite output JSON path (contract §3)"
    )
    parser.add_argument(
        "--round", type=int, required=True, help="interleaved round number"
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="CI profile: the frozen case subset, --measurement-time 2",
    )
    parser.add_argument(
        "--benches",
        default=",".join(bench.name for bench in BENCHES),
        help=(
            "comma-separated bench targets to run (default: all four); local debugging aid, "
            "ab.py never passes it"
        ),
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        benches = select_benches(args.benches)
        side = Side.load(Path(args.side))
    except (ValueError, CommandError, SuiteError) as exc:
        print(f"[rust] error: {exc}", file=sys.stderr, flush=True)
        return 1

    out = SuiteOutput(suite=SUITE, side=side.name, round=args.round, quick=args.quick)
    try:
        collect(side, out, quick=args.quick, benches=benches)
    except (CommandError, SuiteError) as exc:
        print(f"[rust] error: {exc}", file=sys.stderr, flush=True)
        out.write(Path(args.out), ok=False, error=str(exc))
        return 1
    out.write(Path(args.out))
    print(
        f"[rust] {side.name} r{args.round}: {len(out.metrics)} metrics, "
        f"{len(out.notes)} notes → {args.out}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
