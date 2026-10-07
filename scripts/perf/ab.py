#!/usr/bin/env python3
"""perf-ci 的 A/B 编排器（契约 §5 / §6）。

把 merge-base 与 head 两份代码放进**同一次 run**里交错测量：材料化 base worktree、
按 suite 需要构建两侧、逐轮交错跑 `suite_<name>.py`、聚合出中位数与判定、落盘
report JSON（交给 `report.py` 渲染成 PR 评论）。

    # 本地全量（两轮，quick 档）
    uv run python scripts/perf/ab.py --base-ref origin/develop --suites all --rounds 2 --quick
    # 噪声地板（两侧同 rev）
    uv run python scripts/perf/ab.py --base-ref HEAD --calibrate --suites tui
    # 离线自测（不构建、不联网、不建 worktree）
    uv run python scripts/perf/ab.py --selftest

原始单次结果落 `<workdir>/raw/<suite>-<side>-r<round>.json`，子进程日志落
`<workdir>/raw/logs/`；suite 失败只记 `failures[]`（退出码非 0，CI 视为基础设施故障），
其余测量继续；suite 侧写在 raw 里的 `meta.notes` 会带上 `<suite>/<side> r<round>` 标注
汇总进报告的 `meta.notes`（不吞不截断）。rust 的两个质量信号在这里被消费：**载荷指纹**
两侧都采到且不一致 → 该 suite 的指标判 n/a（比的可能不是同一份载荷）；**criterion CI
宽度** > 10% → 加一条提示 note（只提示样本抖动，不改判档）。契约（§Frozen Interfaces）
见 perf-ci 任务书；设计与取舍见 01 harness_core 与 08 ci_calibration 步骤的 `design.md`。
"""

from __future__ import annotations

import argparse
import contextlib
import io
import math
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import report as report_render  # noqa: E402  (同目录模块)
from common import (  # noqa: E402
    CommandError,
    CommandResult,
    Failure,
    Side,
    SuiteOutput,
    SuiteResult,
    Thresholds,
    median,
    metric_unit,
    read_json,
    run_command,
    strip_ansi,
    verdict_for,
    write_json,
)

#: 已知 suite（契约 §2）；脚本 = `scripts/perf/suite_<name>.py`。
SUITE_NAMES: tuple[str, ...] = ("rust", "tui", "gateway")

#: 每个 suite 的构建命令（契约 §5 的 `--prepare`），cwd = side worktree，顺序执行。
#: uv 一律跳过 `wing-cli`（maturin 编译是纯开销：套件用 `target/release/wing` 与
#: `.venv/bin/wing-gateway`，不需要 venv 里的 `wing` 脚本）。
UV_SYNC: tuple[str, ...] = (
    "uv",
    "sync",
    "--frozen",
    "--no-install-package",
    "wing-cli",
)
SUITE_PREPARE: Mapping[str, tuple[tuple[str, ...], ...]] = {
    "rust": (("cargo", "bench", "--no-run", "-p", "wing"),),
    "tui": (("cargo", "build", "--release", "-p", "wing"), UV_SYNC),
    "gateway": (UV_SYNC,),
}

#: 需要 side venv（`<worktree>/.venv/bin/python`）的 suite：缺 venv 直接明确报错。
SUITE_NEEDS_VENV: frozenset[str] = frozenset({"tui", "gateway"})

#: `[perf] run <suite>/<side> r<n>: …` 的首行格式（自测据此断言交错顺序）。
RUN_LINE = re.compile(r"^\[perf\] run (?P<suite>\S+)/(?P<side>\S+) r(?P<round>\d+):")


# ── CLI ───────────────────────────────────────────────────────


def _positive_int(raw: str) -> int:
    value = int(raw)
    if value < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return value


def parse_suites(raw: str) -> tuple[str, ...]:
    """`--suites rust,tui` / `--suites all`；未知名字立刻报错（不静默跑一半）。"""
    names = [part.strip() for part in raw.split(",") if part.strip()]
    if not names:
        raise CommandError("--suites: no suite selected")
    if "all" in names:
        if len(names) > 1:
            raise CommandError("--suites: 'all' cannot be combined with explicit names")
        return SUITE_NAMES
    unknown = [name for name in names if name not in SUITE_NAMES]
    if unknown:
        raise CommandError(
            f"--suites: unknown suite(s) {', '.join(unknown)}; "
            f"known: {', '.join(SUITE_NAMES)} (or 'all')"
        )
    ordered: list[str] = []
    for name in names:
        if name not in ordered:
            ordered.append(name)
    return tuple(ordered)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="ab.py",
        description=(
            "perf-ci A/B harness: measure a merge-base side and the head side in one "
            "interleaved run and emit the wing-perf report JSON"
        ),
        epilog=(
            "local example: uv run python scripts/perf/ab.py "
            "--base-ref origin/develop --suites all --rounds 2 --quick"
        ),
    )
    parser.add_argument(
        "--repo",
        help="head worktree to measure (default: this script's repository root)",
    )
    parser.add_argument(
        "--base-ref",
        dest="base_ref",
        help="git ref/sha for the base side (required unless --selftest)",
    )
    parser.add_argument(
        "--workdir",
        help="scratch dir for the base worktree + raw results (default: <repo>/target/perf)",
    )
    parser.add_argument(
        "--suites",
        default="all",
        help=f"comma-separated suites or 'all' (default: all = {','.join(SUITE_NAMES)})",
    )
    parser.add_argument(
        "--rounds",
        type=_positive_int,
        default=2,
        help="interleaved rounds per suite (default: 2)",
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="CI profile: pass --quick to every suite (bounded work per side)",
    )
    parser.add_argument(
        "--prepare",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="build both sides as the selected suites require (default: on)",
    )
    parser.add_argument(
        "--calibrate",
        action="store_true",
        help="measure both sides at the same rev (noise floor)",
    )
    parser.add_argument(
        "--out-json",
        dest="out_json",
        help="report JSON path (default: <workdir>/ab.json)",
    )
    parser.add_argument(
        "--out-md",
        dest="out_md",
        help="comment markdown path (default: <workdir>/comment.md)",
    )
    parser.add_argument(
        "--selftest",
        action="store_true",
        help="offline self-test with built-in stub suites (no build, no network)",
    )
    args = parser.parse_args(argv)
    if not args.selftest and not args.base_ref:
        parser.error("--base-ref is required (except with --selftest)")
    return args


# ── git / worktree ────────────────────────────────────────────


@dataclass(frozen=True)
class Revs:
    base_ref: str
    head_ref: str
    base_sha: str
    head_sha: str


def git(repo: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=False
    )
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout).strip().splitlines()
        raise CommandError(
            f"git {' '.join(args)} failed in {repo}: {detail[0] if detail else '?'}"
        )
    return proc.stdout.strip()


def resolve_revs(repo: Path, base_ref: str, *, calibrate: bool) -> Revs:
    """解析两侧 rev；`--calibrate` 时两侧都钉在 head（噪声地板）。"""
    head_sha = git(repo, "rev-parse", "HEAD")
    head_ref = git(repo, "rev-parse", "--abbrev-ref", "HEAD")
    base_sha = git(repo, "rev-parse", "--verify", f"{base_ref}^{{commit}}")
    if calibrate:
        base_sha = head_sha
    return Revs(
        base_ref=base_ref, head_ref=head_ref, base_sha=base_sha, head_sha=head_sha
    )


def materialize_worktree(repo: Path, path: Path, sha: str) -> None:
    """把 `path` 指向 `sha`：已就位就复用，否则摘掉重挂（可重入、失败重跑不炸）。"""
    if path.exists():
        current = subprocess.run(
            ["git", "-C", str(path), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )
        if current.returncode == 0 and current.stdout.strip() == sha:
            return
        print(f"[perf] re-pointing {path} → {sha[:12]}", flush=True)
        subprocess.run(
            ["git", "-C", str(repo), "worktree", "remove", "--force", str(path)],
            capture_output=True,
            text=True,
            check=False,
        )
        if path.exists():
            shutil.rmtree(path, ignore_errors=True)
    git(repo, "worktree", "prune")
    path.parent.mkdir(parents=True, exist_ok=True)
    git(repo, "worktree", "add", "--detach", str(path), sha)


def side_for(name: str, worktree: Path, workdir: Path) -> Side:
    """两侧共用同一套路径约定（契约 §1）；只有 worktree 来源不同。"""
    return Side(
        name=name,
        worktree=worktree,
        wing_bin=worktree / "target" / "release" / "wing",
        gateway_bin=worktree / ".venv" / "bin" / "wing-gateway",
        python=worktree / ".venv" / "bin" / "python",
        wing_home=workdir / "wing-home" / name,
    )


# ── prepare ───────────────────────────────────────────────────


def make_preparer(
    *, sides: Mapping[str, Side], log_dir: Path, enabled: bool
) -> Callable[[str], list[Failure]]:
    """构造 per-suite 的 prepare 钩子；返回非空 = 该 suite 跳过（其余继续）。

    同一条命令在一侧只跑一次（tui / gateway 共享 `uv sync`）；`--no-prepare` 只跳过
    构建，仍然校验 suite 必需的 venv 是否存在——否则会拿着别的解释器跑出假数据。
    """
    ran: set[tuple[str, tuple[str, ...]]] = set()

    def prepare(suite: str) -> list[Failure]:
        failures: list[Failure] = []
        for side in sides.values():
            side_failed = False
            for index, cmd in enumerate(SUITE_PREPARE[suite], start=1):
                key = (side.name, cmd)
                if not enabled or key in ran:
                    continue
                # 一条命令一份日志：tui 的 cargo 与 uv 曾互相覆盖（`prepare-<suite>-<side>.log`）。
                label = re.sub(r"[^A-Za-z0-9._-]", "_", cmd[0])
                log_path = log_dir / f"prepare-{suite}-{side.name}-{index}-{label}.log"
                try:
                    result = run_command(
                        cmd, cwd=side.worktree, log_path=log_path, echo=True
                    )
                except CommandError as exc:
                    failures.append(
                        Failure(suite=suite, side=side.name, round=None, error=str(exc))
                    )
                    side_failed = True
                    break
                if result.rc != 0:
                    failures.append(
                        Failure(
                            suite=suite,
                            side=side.name,
                            round=None,
                            error=f"prepare `{' '.join(cmd)}` exited {result.rc} (log: {log_path})",
                            exit_code=result.rc,
                            log=str(log_path),
                            log_tail=result.tail,
                        )
                    )
                    side_failed = True
                    break
                ran.add(key)
            if (
                not side_failed
                and suite in SUITE_NEEDS_VENV
                and not side.python.is_file()
            ):
                failures.append(
                    Failure(
                        suite=suite,
                        side=side.name,
                        round=None,
                        error=(
                            f"side {side.name}: missing venv python {side.python} — "
                            "run without --no-prepare (uv sync --frozen) or create it first"
                        ),
                    )
                )
        return failures

    return prepare


# ── suite 调用与轮次编排 ───────────────────────────────────────


@dataclass
class SubprocessInvoker:
    """跑一次 suite：`<python> suite_<name>.py --side <side.json> --out … --round … [--quick]`。"""

    script_dir: Path
    side_json: Mapping[str, Path]
    raw_dir: Path
    quick: bool = False

    def script_for(self, suite: str) -> Path:
        return self.script_dir / f"suite_{suite}.py"

    def __call__(self, suite: str, side: Side, round_no: int) -> SuiteResult | Failure:
        script = self.script_for(suite)
        out = self.raw_dir / f"{suite}-{side.name}-r{round_no}.json"
        log_path = self.raw_dir / "logs" / f"{suite}-{side.name}-r{round_no}.log"
        if not script.is_file():
            return Failure(
                suite=suite,
                side=side.name,
                round=round_no,
                error=f"suite script not found: {script}",
            )
        out.unlink(missing_ok=True)
        cmd = [
            sys.executable,
            str(script),
            "--side",
            str(self.side_json[side.name]),
            "--out",
            str(out),
            "--round",
            str(round_no),
        ]
        if self.quick:
            cmd.append("--quick")
        try:
            result = run_command(cmd, log_path=log_path)
        except CommandError as exc:
            return Failure(suite=suite, side=side.name, round=round_no, error=str(exc))
        print(
            f"[perf] run {suite}/{side.name} r{round_no}: "
            f"rc={result.rc} in {result.seconds}s ({out.name})",
            flush=True,
        )
        return self._interpret(suite, side, round_no, out, log_path, result)

    def _interpret(
        self,
        suite: str,
        side: Side,
        round_no: int,
        out: Path,
        log_path: Path,
        result: CommandResult,
    ) -> SuiteResult | Failure:
        base: dict[str, Any] = {"suite": suite, "side": side.name, "round": round_no}
        parsed: SuiteResult | None = None
        parse_error: str | None = None
        if out.is_file():
            try:
                parsed = SuiteResult.parse(read_json(out), out)
            except (ValueError, CommandError) as exc:
                parse_error = str(exc)
        if result.rc != 0:
            detail = ""
            if parsed is not None and parsed.error:
                detail = f": {parsed.error}"
            elif parse_error:
                detail = f": {parse_error}"
            return Failure(
                **base,
                error=f"suite exited {result.rc}{detail}",
                exit_code=result.rc,
                log=str(log_path),
                log_tail=result.tail,
            )
        if parsed is None:
            detail = parse_error or f"no output at {out}"
            return Failure(
                **base,
                error=f"suite exited 0 but produced no usable output: {detail}",
                exit_code=0,
                log=str(log_path),
                log_tail=result.tail,
            )
        if not parsed.ok:
            return Failure(
                **base,
                error=f"suite reported ok=false: {parsed.error or 'no reason given'}",
                exit_code=0,
                log=str(log_path),
                log_tail=result.tail,
            )
        if (parsed.suite, parsed.side, parsed.round) != (suite, side.name, round_no):
            return Failure(
                **base,
                error=(
                    "suite output mismatch: "
                    f"suite={parsed.suite} side={parsed.side} round={parsed.round}"
                ),
                log=str(log_path),
            )
        return parsed


def orchestrate(
    *,
    suites: Sequence[str],
    sides: Mapping[str, Side],
    rounds: int,
    invoker: Callable[[str, Side, int], SuiteResult | Failure],
    prepare: Callable[[str], list[Failure]],
) -> tuple[list[SuiteResult], list[Failure]]:
    """契约 §5：suite 顺序执行；每 suite 内 `for r { base; head }`（交错抗漂移）。"""
    results: list[SuiteResult] = []
    failures: list[Failure] = []
    for suite in suites:
        prep_failures = prepare(suite)
        if prep_failures:
            failures.extend(prep_failures)
            print(f"[perf] skip suite {suite}: prepare failed", flush=True)
            continue
        print(f"[perf] suite {suite}: constructed for both sides", flush=True)
        for round_no in range(1, rounds + 1):
            for side in sides.values():
                outcome = invoker(suite, side, round_no)
                if isinstance(outcome, Failure):
                    failures.append(outcome)
                    print(
                        f"[perf] FAIL {suite}/{side.name} r{round_no}: "
                        f"{outcome.error.splitlines()[0]}",
                        flush=True,
                    )
                else:
                    results.append(outcome)
    return results, failures


# ── 聚合与判定（契约 §5/§6） ────────────────────────────────────

#: criterion 置信区间相对宽度超过它 → 聚合一条提示 note（rust 族专有信号）。
CI_WIDTH_WARN_PCT = 10.0

#: 提示 note 里最多列几个指标。
CI_WIDTH_NOTE_LIMIT = 5


def _as_float(value: Any) -> float | None:
    """有限数值 → float；其余（bool / 字符串 / NaN / inf）→ None。"""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    return number if math.isfinite(number) else None


def _fmt_number(value: float) -> str:
    """note 里的数字显示（`%g`：0 / -12.5 / 1.2e+06 都读得出来）。"""
    return f"{value:g}"


def _config_of(result: SuiteResult) -> Mapping[str, Any]:
    config = result.meta.get("config")
    return config if isinstance(config, Mapping) else {}


def _payload_fingerprints(result: SuiteResult) -> Mapping[str, Any]:
    fingerprints = _config_of(result).get("payload_fingerprints")
    return fingerprints if isinstance(fingerprints, Mapping) else {}


def fingerprint_mismatches(results: Sequence[SuiteResult]) -> dict[str, str]:
    """suite → 细节：两侧都采到载荷指纹且不一致（"比的可能不是同一份载荷"）。

    指纹由 bench 打印、suite 收进 `meta.config.payload_fingerprints`（载荷大小 → hash）。
    **仅单侧有**（如首 PR 的 base 缺 bench）不算不一致——那是"没测"，不是"测得不一样"，
    拿它告警会在每个新 bench 的首 PR 上刷假警报。
    """
    per_suite: dict[str, dict[str, dict[str, Any]]] = {}
    for result in results:
        found = _payload_fingerprints(result)
        if found:
            per_suite.setdefault(result.suite, {}).setdefault(result.side, {}).update(
                found
            )
    mismatches: dict[str, str] = {}
    for suite, sides in per_suite.items():
        base, head = sides.get("base", {}), sides.get("head", {})
        if not base or not head:
            continue
        differing = [
            size
            for size in sorted(set(base) | set(head), key=lambda item: str(item))
            if base.get(size) != head.get(size)
        ]
        if not differing:
            continue
        parts = []
        for size in differing[:3]:
            base_hash = (
                base.get(size, {}).get("fnv1a")
                if isinstance(base.get(size), Mapping)
                else None
            )
            head_hash = (
                head.get(size, {}).get("fnv1a")
                if isinstance(head.get(size), Mapping)
                else None
            )
            parts.append(f"n={size} base={base_hash} head={head_hash}")
        more = "" if len(differing) <= 3 else f"（共 {len(differing)} 项）"
        mismatches[suite] = "；".join(parts) + more
    return mismatches


def ci_width_notes(results: Sequence[SuiteResult]) -> list[str]:
    """任一侧 criterion 相对宽度 > 10% 的指标 → 一条提示 note（不改判档）。

    宽度大 = criterion 自己说"这个中位数的置信区间很宽"，值得让人知道；它与"回归"是两件事，
    所以只提示、不进判定（否则等于第二套阈值）。只有 rust 族写入这个信号。
    """
    widths: dict[str, float] = {}
    for result in results:
        raw = _config_of(result).get("ci_rel_pct")
        if not isinstance(raw, Mapping):
            continue
        for metric, value in raw.items():
            number = _as_float(value)
            if number is None or number <= CI_WIDTH_WARN_PCT:
                continue
            widths[str(metric)] = max(widths.get(str(metric), 0.0), number)
    if not widths:
        return []
    ordered = sorted(widths.items(), key=lambda item: item[1], reverse=True)
    shown = "、".join(
        f"{metric} {value:.1f}%" for metric, value in ordered[:CI_WIDTH_NOTE_LIMIT]
    )
    more = f"（共 {len(ordered)} 项）" if len(ordered) > CI_WIDTH_NOTE_LIMIT else ""
    return [
        f"criterion 置信区间偏宽（> {CI_WIDTH_WARN_PCT:.0f}%{more}）：{shown}"
        " —— 只提示样本抖动，不改判档"
    ]


def build_comparisons(
    results: Sequence[SuiteResult], thresholds: Thresholds
) -> tuple[list[dict[str, Any]], list[str]]:
    """每轮代表值 → 轮间中位数 → delta% → 判定；不可比 / 不判定的也显式留下（verdict=n/a）。"""
    notes: list[str] = []
    mismatched = fingerprint_mismatches(results)
    for suite, detail in sorted(mismatched.items()):
        notes.append(
            f"{suite}: 两侧载荷指纹不一致（bench 载荷生成器被改过？该 suite 的指标不可比）"
            f"：{detail}"
        )
    notes.extend(ci_width_notes(results))
    table: dict[str, dict[str, dict[int, float]]] = {}
    suites: dict[str, str] = {}
    for result in results:
        for metric in sorted(set(result.metrics) | set(result.samples)):
            value = result.round_value(metric)
            if value is None:
                notes.append(
                    f"{result.suite}/{result.side} r{result.round}: 指标 {metric} 无数值"
                    "（缺失或非有限），本轮不计入"
                )
                continue
            suites.setdefault(metric, result.suite)
            table.setdefault(metric, {}).setdefault(result.side, {})[result.round] = (
                value
            )

    comparisons: list[dict[str, Any]] = []
    for metric in sorted(table):
        suite = suites[metric]
        if metric.split(".", 1)[0] != suite:
            notes.append(
                f"指标 {metric} 由 suite {suite} 产出（前缀与 suite 名不一致）"
            )
        per_side = table[metric]
        noise, regress = thresholds.for_metric(metric, suite)
        medians: dict[str, float | None] = {}
        entry: dict[str, Any] = {"metric": metric, "suite": suite}
        for side_name in ("base", "head"):
            values = [
                value for _round, value in sorted(per_side.get(side_name, {}).items())
            ]
            entry[side_name] = [round(value, 6) for value in values]
            medians[side_name] = median(values) if values else None
        base_median, head_median = medians["base"], medians["head"]
        entry["base_median"] = None if base_median is None else round(base_median, 6)
        entry["head_median"] = None if head_median is None else round(head_median, 6)
        entry["delta_pct"] = None
        entry["verdict"] = "n/a"
        entry["noise_pct"] = noise
        entry["regress_pct"] = regress
        note: str | None = None
        if base_median is None or head_median is None:
            missing = "base" if base_median is None else "head"
            note = f"{missing} 侧无数据"
        elif base_median <= 0:
            # 负基准与零基准是同一类问题：百分比在 base ≤ 0 时没有方向含义
            # （有符号量的 ±Δ 会翻转符号，零基准则无定义）。
            note = f"基准非正（base 中位数 {_fmt_number(base_median)}）：Δ% 无定义"
        else:
            delta = round(100.0 * (head_median - base_median) / base_median, 3)
            entry["delta_pct"] = delta
            if suite in mismatched:
                note = "两侧载荷指纹不一致：比的可能不是同一份载荷"
            elif thresholds.is_info_only(metric):
                # 测量质量信息项（例：TUI 覆盖率、零中心的有符号 trend）：数值照给，
                # 但不判档 —— 覆盖率下降不等于性能变差，标成 improved 是错的。
                note = "测量质量信息项（不参与判定）"
            else:
                entry["verdict"] = verdict_for(
                    delta,
                    noise_pct=noise,
                    regress_pct=regress,
                    higher_better=thresholds.is_higher_better(metric),
                )
        if note:
            entry["note"] = note
        comparisons.append(entry)

    unequal = [
        str(entry["metric"])
        for entry in comparisons
        if entry["base"] and entry["head"] and len(entry["base"]) != len(entry["head"])
    ]
    if unequal:
        shown = ", ".join(unequal[:4]) + (" …" if len(unequal) > 4 else "")
        notes.append(f"{len(unequal)} 个指标两侧轮数不等（有轮次失败）：{shown}")
    return comparisons, notes


def suite_notes(results: Sequence[SuiteResult]) -> list[str]:
    """把各侧 raw 输出的 `meta.notes` 汇总进编排层的 notes（标注 suite/side/round）。

    suite 的软异常（case n/a、采样不足、帧数对不上…）只落在单侧 raw JSON 里，PR 评论
    看不到就是"有样本被守卫拒了"没人知道。这里逐条带上定位信息原样透传，不做去重与
    截断（同一 note 出现在两个 round 就是两个事实）；文本过一遍 `strip_ansi` 兜底——
    CI 上 cargo 的彩色 stderr 曾把 `\x1b[…` 直接带进评论。
    """
    notes: list[str] = []
    for result in results:
        for note in result.notes:
            notes.append(
                f"{result.suite}/{result.side} r{result.round}: {strip_ansi(note)}"
            )
    return notes


@dataclass(frozen=True)
class RunContext:
    """报告头部的元信息（契约 §6 的顶层字段来源）。"""

    repo: Path
    workdir: Path
    base_ref: str
    head_ref: str
    base_sha: str
    head_sha: str
    suites: tuple[str, ...]
    rounds: int
    quick: bool
    calibrate: bool
    prepared: bool
    stub: bool


def build_report(
    ctx: RunContext,
    comparisons: Sequence[Mapping[str, Any]],
    failures: Sequence[Failure],
    notes: Sequence[str],
    duration_s: float,
) -> dict[str, Any]:
    all_notes = list(notes)
    if ctx.calibrate:
        all_notes.insert(0, f"calibrate：两侧同 rev {ctx.head_sha[:12]}（噪声地板）")
    if ctx.rounds == 1:
        all_notes.append("rounds=1：没有轮间离散度，判定只看单轮差异")
    return {
        "kind": "wing-perf",
        "shape": 1,
        "base_ref": ctx.base_ref,
        "head_ref": ctx.head_ref,
        "base_sha": ctx.base_sha,
        "head_sha": ctx.head_sha,
        "rounds": ctx.rounds,
        "quick": ctx.quick,
        "calibrate": ctx.calibrate,
        "comparisons": list(comparisons),
        "failures": [failure.to_json() for failure in failures],
        "meta": {
            "duration_s": round(duration_s, 1),
            "notes": all_notes,
            "suites": list(ctx.suites),
            "repo": str(ctx.repo),
            "workdir": str(ctx.workdir),
            "raw_dir": str(ctx.workdir / "raw"),
            "prepared": ctx.prepared,
            "stub": ctx.stub,
        },
    }


def exit_code(report: Mapping[str, Any]) -> int:
    """契约 §5：任何失败 → 非 0（CI 视为基础设施故障）；否则 0。"""
    return 1 if report.get("failures") else 0


# ── 主流程 ─────────────────────────────────────────────────────


def run_ab(args: argparse.Namespace) -> int:
    started = time.monotonic()
    repo = Path(args.repo).expanduser().resolve() if args.repo else REPO_ROOT
    if not (repo / "Cargo.toml").is_file() or not (HERE / "common.py").is_file():
        raise CommandError(f"--repo {repo}: not a wing checkout (missing Cargo.toml)")
    suites = parse_suites(args.suites)
    workdir = (
        Path(args.workdir).expanduser().resolve()
        if args.workdir
        else repo / "target" / "perf"
    )
    raw_dir = workdir / "raw"
    raw_dir.mkdir(parents=True, exist_ok=True)
    (workdir / "sides").mkdir(parents=True, exist_ok=True)
    log_dir = raw_dir / "logs"
    notes: list[str] = []

    revs = resolve_revs(repo, args.base_ref, calibrate=args.calibrate)
    if git(repo, "status", "--porcelain"):
        notes.append("head worktree 有未提交改动：head 数字对应工作区，不对应任何 sha")
    base_worktree = workdir / "base"
    materialize_worktree(repo, base_worktree, revs.base_sha)
    sides = {
        "base": side_for("base", base_worktree, workdir),
        "head": side_for("head", repo, workdir),
    }
    side_json: dict[str, Path] = {}
    for name, side in sides.items():
        side.wing_home.mkdir(parents=True, exist_ok=True)
        path = workdir / "sides" / f"{name}.json"
        side.write(path)
        side_json[name] = path

    print(
        f"[perf] base={revs.base_sha[:12]} head={revs.head_sha[:12]} "
        f"suites={','.join(suites)} rounds={args.rounds} quick={args.quick} "
        f"prepare={args.prepare} calibrate={args.calibrate}",
        flush=True,
    )
    results, failures = orchestrate(
        suites=suites,
        sides=sides,
        rounds=args.rounds,
        invoker=SubprocessInvoker(
            script_dir=HERE, side_json=side_json, raw_dir=raw_dir, quick=args.quick
        ),
        prepare=make_preparer(sides=sides, log_dir=log_dir, enabled=args.prepare),
    )
    thresholds = Thresholds.load(HERE / "thresholds.json")
    comparisons, agg_notes = build_comparisons(results, thresholds)
    notes.extend(agg_notes)
    notes.extend(suite_notes(results))
    ctx = RunContext(
        repo=repo,
        workdir=workdir,
        base_ref=revs.base_ref,
        head_ref=revs.head_ref,
        base_sha=revs.base_sha,
        head_sha=revs.head_sha,
        suites=suites,
        rounds=args.rounds,
        quick=args.quick,
        calibrate=args.calibrate,
        prepared=args.prepare,
        stub=False,
    )
    payload = build_report(
        ctx, comparisons, failures, notes, time.monotonic() - started
    )

    out_json = (
        Path(args.out_json).expanduser() if args.out_json else workdir / "ab.json"
    )
    out_md = Path(args.out_md).expanduser() if args.out_md else workdir / "comment.md"
    write_json(out_json, payload)
    out_md.parent.mkdir(parents=True, exist_ok=True)
    out_md.write_text(report_render.render_comment(payload), encoding="utf-8")
    _print_summary(payload, out_json, out_md, workdir)
    return exit_code(payload)


def _print_summary(
    payload: Mapping[str, Any], out_json: Path, out_md: Path, workdir: Path
) -> None:
    comparisons = [
        entry for entry in payload.get("comparisons", []) if isinstance(entry, Mapping)
    ]
    failures = payload.get("failures") or []
    ordered = sorted(
        comparisons,
        key=lambda entry: abs(float(entry.get("delta_pct") or 0.0)),
        reverse=True,
    )
    print(
        f"[perf] {len(comparisons)} metrics · {len(failures)} failures",
        flush=True,
    )
    for entry in ordered[:40]:
        delta = entry.get("delta_pct")
        text = "n/a" if delta is None else f"{float(delta):+.1f}%"
        print(
            f"[perf]   {str(entry.get('verdict')):<10} {text:>9} {entry.get('metric')}"
        )
    if len(ordered) > 40:
        print(f"[perf]   … {len(ordered) - 40} more (see the JSON)", flush=True)
    for failure in failures:
        print(
            f"[perf]   failure {failure.get('suite')}/{failure.get('side')}: {failure.get('error')}"
        )
    print(f"[perf] wrote {out_json}", flush=True)
    print(f"[perf] wrote {out_md}", flush=True)
    base_worktree = shlex.quote(str(workdir / "base"))
    scratch = shlex.quote(str(workdir))
    print(
        f"[perf] scratch: {scratch} (base worktree: {base_worktree}) — reuse it for the next "
        f"run, or clean up with `git -C <repo> worktree remove --force {base_worktree}` "
        f"+ `rm -rf {scratch}`",
        flush=True,
    )


# ── 自测（离线、不构建、不联网、不建 worktree） ─────────────────


STUB_SUITE = '''#!/usr/bin/env python3
"""ab.py --selftest 生成的确定性 stub suite：内置数据、不落真实测量、不入库。"""

import argparse
import json
import sys
import time
from pathlib import Path

MODE = "__MODE__"
SUITE = "__SUITE__"
SIDE_FIELDS = ("name", "worktree", "wing_bin", "gateway_bin", "python", "wing_home")
#: (case, base 各轮代表值, head 各轮代表值)：构造 flat / improved / watch / regression 四档。
#: `steady` / `skew` 的轮值刻意不对称（均值 != 中位数）：把聚合口径换成均值会翻档 / 改数。
CASES = (
    ("steady", (10.0, 10.0, 10.0), (10.6, 10.0, 9.8)),
    ("better", (10.0, 10.0, 10.0), (9.5, 9.0, 8.5)),
    ("watch", (10.0, 10.0, 10.0), (11.4, 11.5, 11.6)),
    ("worse", (10.0, 10.0, 10.0), (13.0, 14.0, 15.0)),
    ("skew", (10.0, 10.0, 10.0), (9.0, 9.2, 13.0)),
)
#: 只给 samples、不给 metrics 的 case：验证 ab 的「样本中位数」回退路径。
SAMPLES_ONLY = "better"
#: metrics 与样本中位数**刻意**不一致的 case（样本中位数 base=6 / head=10，metrics 一律 8）：
#: 钉住 ab 的「metrics 优先于 samples」规则——真实 suite 应保持两者一致，这里只为判别力。
MISMATCH = "mismatch"
MISMATCH_SAMPLE_MEDIAN = {"base": 6.0, "head": 10.0}


def main() -> int:
    started = time.monotonic()
    parser = argparse.ArgumentParser()
    parser.add_argument("--side", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--round", type=int, required=True)
    parser.add_argument("--quick", action="store_true")
    args = parser.parse_args()
    side = json.loads(Path(args.side).read_text(encoding="utf-8"))
    for key in SIDE_FIELDS:
        if key not in side:
            print(f"stub: side descriptor missing {key!r}", file=sys.stderr)
            return 9
    name = str(side["name"])
    payload = {
        "suite": SUITE,
        "side": name,
        "round": args.round,
        "ok": True,
        "error": None,
        "metrics": {},
        "samples": {},
        "meta": {
            "duration_s": 0.0,
            "notes": [f"stub note for {name} r{args.round}"],
            "config": {"argv": sys.argv[1:], "quick": args.quick},
        },
    }
    if MODE == "fail" and name == "head":
        payload["ok"] = False
        payload["error"] = "stub: injected failure on head"
        _write(Path(args.out), payload)
        print("stub: injected failure on head", file=sys.stderr)
        return 1
    if MODE == "bad_side":
        payload["side"] = f"{name}-wrong"
    if MODE == "bad_round":
        payload["round"] = args.round + 1
    for case, base_values, head_values in CASES:
        values = base_values if name == "base" else head_values
        value = float(values[min(args.round - 1, len(values) - 1)])
        metric = f"{SUITE}.{case}.median_ms"
        payload["samples"][metric] = [round(value * 0.99, 6), value, round(value * 1.01, 6)]
        if case != SAMPLES_ONLY:
            payload["metrics"][metric] = value
    median = MISMATCH_SAMPLE_MEDIAN.get(name, 0.0)
    payload["metrics"][f"{SUITE}.{MISMATCH}.median_ms"] = 8.0
    payload["samples"][f"{SUITE}.{MISMATCH}.median_ms"] = [median, median, median]
    payload["meta"]["duration_s"] = round(time.monotonic() - started, 3)
    _write(Path(args.out), payload)
    return 0


def _write(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\\n", encoding="utf-8")


if __name__ == "__main__":
    raise SystemExit(main())
'''

#: stub 模式 → suite 名（= `suite_<name>.py` 与输出里的 `suite` 字段）。
STUB_MODES: Mapping[str, str] = {
    "ok": "stub",
    "fail": "stubfail",
    "bad_side": "stubside",
    "bad_round": "stubround",
}


class _Checker:
    """自测的断言收集器：失败的项照实打印，汇总决定退出码。"""

    def __init__(self) -> None:
        self.passed = 0
        self.skipped = 0
        self.failed: list[str] = []

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        if ok:
            self.passed += 1
            print(f"[selftest] ok   {name}", flush=True)
        else:
            self.failed.append(name)
            print(f"[selftest] FAIL {name}: {detail}", flush=True)
        return ok

    def skip(self, name: str, why: str) -> None:
        self.skipped += 1
        print(f"[selftest] skip {name}: {why}", flush=True)

    def summary(self) -> int:
        print(
            f"[selftest] {self.passed} passed · {len(self.failed)} failed · {self.skipped} skipped",
            flush=True,
        )
        if self.failed:
            print("[selftest] failed: " + ", ".join(self.failed), flush=True)
            return 1
        return 0


def _write_stub(directory: Path, mode: str, suite: str | None = None) -> Path:
    """生成一个 stub suite（内置数据）；`suite` 覆盖名字（CLI 端到端会写成 `rust`）。"""
    name = suite or STUB_MODES[mode]
    path = directory / f"suite_{name}.py"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        STUB_SUITE.replace("__MODE__", mode).replace("__SUITE__", name),
        encoding="utf-8",
    )
    return path


def _copy_harness(destination: Path) -> Path:
    """把 harness 三件套 + thresholds 复制到临时目录：CLI 端到端自测要在隔离布局里跑。"""
    destination.mkdir(parents=True, exist_ok=True)
    for name in ("ab.py", "common.py", "report.py", "thresholds.json"):
        shutil.copy(HERE / name, destination / name)
    return destination


def _init_scratch_repo(root: Path) -> tuple[str, str]:
    """建一个最小的临时 git 仓库（`Cargo.toml` + 两个提交），返回 (HEAD~1 sha, HEAD sha)。"""
    root.mkdir(parents=True, exist_ok=True)
    (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
    git(root, "-c", "init.defaultBranch=main", "init", "-q")
    git(root, "add", "Cargo.toml")
    _git_commit(root, "first")
    _git_commit(root, "second", allow_empty=True)
    return (git(root, "rev-parse", "HEAD~1"), git(root, "rev-parse", "HEAD"))


def _git_commit(repo: Path, message: str, *, allow_empty: bool = False) -> None:
    """临时仓库里的提交（显式带身份与 gpgsign，别依赖 runner 的全局配置）。"""
    args = [
        "-c",
        "user.email=perf-selftest@invalid",
        "-c",
        "user.name=perf-selftest",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "-m",
        message,
    ]
    if allow_empty:
        args.append("--allow-empty")
    git(repo, *args)


def _fake_side(workdir: Path, name: str) -> Side:
    """stub 侧的 side 描述符：路径都不存在（stub 不做真实测量，只校验契约字段）。"""
    root = workdir / "stub-side" / name
    return Side(
        name=name,
        worktree=root,
        wing_bin=root / "target" / "release" / "wing",
        gateway_bin=root / ".venv" / "bin" / "wing-gateway",
        python=root / ".venv" / "bin" / "python",
        wing_home=root / "wing-home",
    )


def _read_or_empty(path: Path) -> dict[str, Any]:
    """自测用：文件缺失 / 坏 JSON 都返回 `{}`，让断言给出可读的失败而不是崩溃。"""
    if not path.is_file():
        return {}
    try:
        payload = read_json(path)
    except (ValueError, CommandError):
        return {}
    return payload if isinstance(payload, dict) else {}


def _rev_of(path: Path) -> str:
    try:
        return git(path, "rev-parse", "HEAD")
    except CommandError:
        return "<not-a-worktree>"


def _synth(
    suite: str,
    side: str,
    round_no: int,
    metrics: Mapping[str, float],
    *,
    config: Mapping[str, Any] | None = None,
    notes: Sequence[str] = (),
) -> SuiteResult:
    payload = {
        "suite": suite,
        "side": side,
        "round": round_no,
        "ok": True,
        "error": None,
        "metrics": dict(metrics),
        "samples": {},
        "meta": {"notes": list(notes), "config": dict(config or {})},
    }
    return SuiteResult.parse(payload, Path("<synthetic>"))


def _run(cmd: Sequence[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(part) for part in cmd], capture_output=True, text=True, check=False
    )


def _parse_order(text: str) -> list[tuple[str, str, int]]:
    order: list[tuple[str, str, int]] = []
    for line in text.splitlines():
        match = RUN_LINE.match(line)
        if match:
            order.append(
                (match.group("suite"), match.group("side"), int(match.group("round")))
            )
    return order


def run_selftest() -> int:
    checker = _Checker()
    print(
        "[selftest] offline · no cargo/uv · built-in stub data · a scratch git repo in a temp "
        "dir (this worktree is never touched)",
        flush=True,
    )

    thresholds = Thresholds.load(HERE / "thresholds.json")
    checker.check(
        "thresholds.load",
        thresholds.noise_pct > 0 and thresholds.regress_pct > thresholds.noise_pct,
        f"noise={thresholds.noise_pct} regress={thresholds.regress_pct}",
    )
    # 落库的带宽：更具体的模式在前（for_metric 先命中先胜）——顺序写反会静默吃掉覆盖。
    for metric, suite, want in (
        ("tui.display.lag_p50_us", "tui", (15.0, 30.0)),
        ("tui.display.lag_p99_us", "tui", (25.0, 45.0)),
        ("tui.display.lag_max_us", "tui", (25.0, 45.0)),
        ("tui.tui_cpu_ratio", "tui", (30.0, 60.0)),
        ("gateway.fanout.complete_ms", "gateway", (15.0, 30.0)),
        ("gateway.fanout.gap_p99_us", "gateway", (30.0, 50.0)),
        ("gateway.resume.sync_ms", "gateway", (15.0, 30.0)),
        # µs 级 bench：CI 实测轮间漂移 ~24%（run 37667281765），只给 append 放宽。
        ("rust.tool_args_stream.append.256.median_ns", "rust", (12.0, 30.0)),
        ("rust.tool_args_stream.append.512.median_ns", "rust", (12.0, 30.0)),
        ("rust.tool_args_stream.frames_60fps.256.median_ns", "rust", (6.0, 20.0)),
        ("rust.session_replay.replay.1000.median_ns", "rust", (6.0, 20.0)),
    ):
        checker.check(
            f"thresholds.file {metric}",
            thresholds.for_metric(metric, suite) == want,
            f"{thresholds.for_metric(metric, suite)} != {want}",
        )
    checker.check(
        "thresholds.file.info_only",
        all(
            thresholds.is_info_only(metric)
            for metric in (
                "tui.display.coverage_ratio",
                "tui.display.trend_us",
                "gateway.turn.p99_ms",
                "gateway.context.build_p99_ms",
            )
        )
        and not thresholds.is_info_only("tui.display.lag_p50_us")
        and not thresholds.is_info_only("gateway.turn.p50_ms")
        and not thresholds.higher_better,
        f"info_only={thresholds.info_only} higher_better={thresholds.higher_better}",
    )
    for delta, want in (
        (0.0, "flat"),
        (3.0, "flat"),
        (-5.9, "flat"),
        (5.999, "flat"),  # |Δ| == noise 的紧邻下侧
        (6.0, "watch"),  # 契约：|Δ| == noise_pct 不再算 flat（劣化侧 → watch）
        (-6.0, "improved"),  # 同上，变好侧 → improved
        (-10.0, "improved"),
        (15.0, "watch"),
        (19.9, "watch"),
        (20.0, "regression"),
        (40.0, "regression"),
        (-40.0, "improved"),
    ):
        got = verdict_for(delta, noise_pct=6.0, regress_pct=20.0, higher_better=False)
        checker.check(f"verdict {delta:+g}% → {want}", got == want, f"got {got}")
    for delta, want in ((15.0, "improved"), (-25.0, "regression"), (5.0, "flat")):
        got = verdict_for(delta, noise_pct=6.0, regress_pct=20.0, higher_better=True)
        checker.check(
            f"verdict higher-better {delta:+g}% → {want}", got == want, f"got {got}"
        )

    overridden = Thresholds(
        noise_pct=6.0,
        regress_pct=20.0,
        overrides={"tui.*": {"noise_pct": 10.0}, "gateway": {"regress_pct": 30.0}},
        higher_better=("tui.*ratio",),
    )
    checker.check(
        "thresholds.override.metric",
        overridden.for_metric("tui.display.lag_p50_us", "tui") == (10.0, 20.0),
        str(overridden.for_metric("tui.display.lag_p50_us", "tui")),
    )
    checker.check(
        "thresholds.override.suite",
        overridden.for_metric("gateway.fanout.complete_ms", "gateway") == (6.0, 30.0),
        str(overridden.for_metric("gateway.fanout.complete_ms", "gateway")),
    )
    checker.check(
        "thresholds.override.default",
        overridden.for_metric("rust.session_replay.replay.1000.median_ns", "rust")
        == (6.0, 20.0),
        str(overridden.for_metric("rust.session_replay.replay.1000.median_ns", "rust")),
    )
    checker.check(
        "thresholds.higher_better",
        overridden.is_higher_better("tui.coverage_ratio")
        and not overridden.is_higher_better("tui.display.lag_p50_us"),
    )
    checker.check(
        "thresholds.defaults.info_only",
        Thresholds().info_only == ()
        and not Thresholds().is_info_only("tui.display.coverage_ratio"),
        str(Thresholds().info_only),
    )
    checker.check(
        "suites.all", parse_suites("all") == SUITE_NAMES, str(parse_suites("all"))
    )
    checker.check("suites.explicit", parse_suites("tui,gateway") == ("tui", "gateway"))
    checker.check(
        "metric_unit.suffixes",
        metric_unit("stub.worse.median_ms") == "ms"
        and metric_unit("tui.display.lag_p50_us") == "µs"
        and metric_unit("rust.session_replay.replay.1000.median_ns") == "ns"
        and metric_unit("tui.coverage_ratio") == ""
        and metric_unit("gateway.cpu_pct") == "%"
        and metric_unit("gateway.fanout.cpu_ms_per_1k") == "ms/1k"
        and metric_unit("weird.metric") == "",
        " / ".join(
            metric_unit(metric)
            for metric in (
                "stub.worse.median_ms",
                "tui.display.lag_p50_us",
                "gateway.fanout.cpu_ms_per_1k",
                "weird.metric",
            )
        ),
    )
    checker.check(
        "strip_ansi.csi",
        strip_ansi("\x1b[1m\x1b[91merror\x1b[0m: no bench target named `x`")
        == "error: no bench target named `x`"
        and strip_ansi("plain text") == "plain text"
        and strip_ansi("a\x1b[0mb") == "ab",
        repr(strip_ansi("\x1b[1m\x1b[91merror\x1b[0m: no bench target")),
    )
    checker.check(
        "fmt_value.ladder",
        report_render.fmt_value(0.0) == "0"
        and report_render.fmt_value(14.5) == "14.5"
        and report_render.fmt_value(1000.0) == "1,000"
        and report_render.fmt_value(0.98) == "0.98"
        and report_render.fmt_value(0.000123) == "0.000123"
        and report_render.fmt_value(0.0000123) == "1.23e-05"  # 不许塌成 "0"
        and report_render.fmt_value(None) == "—",
        " / ".join(
            report_render.fmt_value(value)
            for value in (14.5, 0.98, 0.000123, 0.0000123, None)
        ),
    )
    checker.check(
        "report.per_1k_unit",
        "70 ms/1k"
        in report_render._row(
            {
                "metric": "gateway.fanout.cpu_ms_per_1k",
                "base_median": 70.0,
                "head_median": 60.0,
                "delta_pct": -14.3,
                "verdict": "flat",
            }
        ),
        report_render._row(
            {
                "metric": "gateway.fanout.cpu_ms_per_1k",
                "base_median": 70.0,
                "head_median": 60.0,
                "delta_pct": -14.3,
                "verdict": "flat",
            }
        ),
    )

    with tempfile.TemporaryDirectory(prefix="wing-perf-selftest-") as tmp:
        workdir = Path(tmp) / "scratch"
        workdir.mkdir(parents=True, exist_ok=True)

        # 阈值文件的向后兼容：契约 §8 的形状（没有 info_only 键）必须照常加载。
        legacy_path = workdir / "thresholds-legacy.json"
        write_json(legacy_path, {"default": {"noise_pct": 8, "regress_pct": 25}})
        legacy = Thresholds.load(legacy_path)
        checker.check(
            "thresholds.legacy_shape",
            (legacy.noise_pct, legacy.regress_pct) == (8.0, 25.0)
            and legacy.info_only == ()
            and legacy.higher_better == (),
            f"{legacy.noise_pct}/{legacy.regress_pct}/{legacy.info_only}",
        )

        # ① suite 输出构造器 / 解析器（契约 §3）
        shape_path = workdir / "shape.json"
        out = SuiteOutput(suite="stub", side="base", round=1, quick=True)
        out.metric("stub.a.median_ms", 1.5, 1.4, 1.5, 1.6)
        out.sample("stub.b.median_ms", 2.0)
        out.sample("stub.b.median_ms", 2.4)
        out.note("hello")
        out.setting("mode", "shape")
        out.write(shape_path)
        shape = read_json(shape_path)
        checker.check(
            "suite_output.keys",
            sorted(shape)
            == ["error", "meta", "metrics", "ok", "round", "samples", "side", "suite"],
            str(sorted(shape)),
        )
        checker.check(
            "suite_output.meta",
            sorted(shape["meta"]) == ["config", "duration_s", "notes"]
            and shape["meta"]["config"]["quick"] is True,
            str(shape["meta"]),
        )
        parsed = SuiteResult.parse(shape, shape_path)
        checker.check(
            "suite_result.round_value",
            parsed.round_value("stub.a.median_ms") == 1.5
            and parsed.round_value("stub.b.median_ms") == 2.2
            and parsed.round_value("missing") is None,
            f"{parsed.round_value('stub.a.median_ms')} / {parsed.round_value('stub.b.median_ms')}",
        )
        checker.check(
            "suite_result.notes", parsed.notes == ("hello",), str(parsed.notes)
        )
        try:
            SuiteResult.parse({"suite": "x"}, shape_path)
            checker.check(
                "suite_result.reject_bad", False, "accepted a payload without metrics"
            )
        except ValueError:
            checker.check("suite_result.reject_bad", True)

        # ② side 描述符往返 + 缺字段拒绝（契约 §1）
        sides = {
            "base": _fake_side(workdir, "base"),
            "head": _fake_side(workdir, "head"),
        }
        side_json: dict[str, Path] = {}
        for name, side in sides.items():
            path = workdir / "sides" / f"{name}.json"
            side.write(path)
            side_json[name] = path
        checker.check("side.round_trip", Side.load(side_json["base"]) == sides["base"])
        broken = workdir / "sides" / "broken.json"
        write_json(broken, {"name": "base"})
        try:
            Side.load(broken)
            checker.check(
                "side.reject_incomplete", False, "loaded a descriptor with 1 field"
            )
        except ValueError:
            checker.check("side.reject_incomplete", True)

        # ③ 交错轮次矩阵：真子进程跑 stub suite（健康数据）
        stub_dir = workdir / "stub"
        _write_stub(stub_dir, "ok")
        _write_stub(stub_dir, "fail")
        _write_stub(stub_dir, "bad_side")
        _write_stub(stub_dir, "bad_round")
        raw_dir = workdir / "raw"
        invoker = SubprocessInvoker(
            script_dir=stub_dir, side_json=side_json, raw_dir=raw_dir, quick=False
        )
        log = io.StringIO()
        with contextlib.redirect_stdout(log):
            results, failures = orchestrate(
                suites=("stub",),
                sides=sides,
                rounds=3,
                invoker=invoker,
                prepare=lambda suite: [],
            )
        order = _parse_order(log.getvalue())
        checker.check(
            "matrix.interleaved",
            order
            == [
                ("stub", "base", 1),
                ("stub", "head", 1),
                ("stub", "base", 2),
                ("stub", "head", 2),
                ("stub", "base", 3),
                ("stub", "head", 3),
            ],
            str(order),
        )
        checker.check(
            "matrix.clean_run",
            len(results) == 6 and not failures,
            f"{len(results)} results / {len(failures)} failures",
        )
        expected_raw = {
            f"stub-{side}-r{round_no}.json"
            for side in ("base", "head")
            for round_no in (1, 2, 3)
        }
        actual_raw = {path.name for path in raw_dir.glob("stub-*.json")}
        checker.check(
            "matrix.raw_files", actual_raw == expected_raw, str(sorted(actual_raw))
        )
        checker.check(
            "matrix.raw_rounds",
            all(
                read_json(path)["round"] == int(path.stem.rsplit("-r", 1)[1])
                for path in raw_dir.glob("stub-*.json")
            ),
        )

        # ④ 聚合：中位数（不是均值）/ delta 符号 / 四档判定 / 样本回退 / metrics 优先
        comparisons, notes = build_comparisons(results, thresholds)
        by_metric = {str(entry["metric"]): entry for entry in comparisons}
        checker.check(
            "aggregate.metrics",
            set(by_metric)
            == {
                "stub.steady.median_ms",
                "stub.better.median_ms",
                "stub.watch.median_ms",
                "stub.worse.median_ms",
                "stub.skew.median_ms",
                "stub.mismatch.median_ms",
            },
            str(sorted(by_metric)),
        )
        checker.check(
            "aggregate.medians",
            by_metric["stub.worse.median_ms"]["head"] == [13.0, 14.0, 15.0]
            and by_metric["stub.worse.median_ms"]["head_median"] == 14.0
            and by_metric["stub.steady.median_ms"]["base"] == [10.0, 10.0, 10.0],
            str(by_metric["stub.worse.median_ms"]),
        )
        # 轮值刻意不对称：steady 中位数 10.0（均值 10.13）、skew 中位数 9.2（均值 10.4，Δ 会翻档）。
        checker.check(
            "aggregate.median_not_mean",
            by_metric["stub.steady.median_ms"]["head"] == [10.6, 10.0, 9.8]
            and by_metric["stub.steady.median_ms"]["head_median"] == 10.0
            and by_metric["stub.skew.median_ms"]["head_median"] == 9.2
            and abs(float(by_metric["stub.skew.median_ms"]["delta_pct"]) + 8.0) < 1e-6,
            f"{by_metric['stub.steady.median_ms']['head_median']} / "
            f"{by_metric['stub.skew.median_ms']['head_median']}",
        )
        checker.check(
            "aggregate.delta_sign",
            abs(float(by_metric["stub.better.median_ms"]["delta_pct"]) + 10.0) < 1e-6
            and abs(float(by_metric["stub.worse.median_ms"]["delta_pct"]) - 40.0)
            < 1e-6,
            f"{by_metric['stub.better.median_ms']['delta_pct']} / "
            f"{by_metric['stub.worse.median_ms']['delta_pct']}",
        )
        # metrics 与样本中位数刻意不一致（8.0 vs 6.0 / 10.0）：优先 samples 会把 flat 翻成 regression。
        checker.check(
            "aggregate.metrics_over_samples",
            by_metric["stub.mismatch.median_ms"]["base"] == [8.0, 8.0, 8.0]
            and by_metric["stub.mismatch.median_ms"]["head"] == [8.0, 8.0, 8.0]
            and by_metric["stub.mismatch.median_ms"]["verdict"] == "flat",
            str(by_metric["stub.mismatch.median_ms"]),
        )
        verdicts = {metric: entry["verdict"] for metric, entry in by_metric.items()}
        checker.check(
            "aggregate.verdicts",
            verdicts
            == {
                "stub.steady.median_ms": "flat",
                "stub.better.median_ms": "improved",
                "stub.watch.median_ms": "watch",
                "stub.worse.median_ms": "regression",
                "stub.skew.median_ms": "improved",
                "stub.mismatch.median_ms": "flat",
            },
            str(verdicts),
        )
        checker.check(
            "aggregate.samples_fallback",
            by_metric["stub.better.median_ms"]["head"] == [9.5, 9.0, 8.5],
            str(by_metric["stub.better.median_ms"]["head"]),
        )
        checker.check("aggregate.no_notes", not notes, str(notes))
        propagated = suite_notes(results)
        checker.check(
            "notes.propagation",
            propagated
            == [
                f"stub/{side_name} r{round_no}: stub note for {side_name} r{round_no}"
                for round_no in (1, 2, 3)
                for side_name in ("base", "head")
            ],
            str(propagated),
        )
        ansi_note = suite_notes(
            [
                _synth(
                    "rust",
                    "base",
                    1,
                    {"rust.x.median_ns": 1.0},
                    notes=[
                        "session_replay: bench target missing on side base "
                        "(\x1b[1m\x1b[91merror\x1b[0m: no bench target named `session_replay`)"
                    ],
                )
            ]
        )
        checker.check(
            "notes.ansi_stripped",
            ansi_note
            == [
                "rust/base r1: session_replay: bench target missing on side base "
                "(error: no bench target named `session_replay`)"
            ]
            and "\x1b" not in ansi_note[0],
            repr(ansi_note),
        )

        ctx = RunContext(
            repo=REPO_ROOT,
            workdir=workdir,
            base_ref="HEAD~1",
            head_ref="HEAD",
            base_sha="b" * 40,
            head_sha="c" * 40,
            suites=("stub",),
            rounds=3,
            quick=False,
            calibrate=False,
            prepared=False,
            stub=True,
        )
        payload = build_report(
            ctx,
            comparisons,
            failures,
            ["head worktree 有未提交改动：head 数字对应工作区", *notes, *propagated],
            1.0,
        )
        checker.check("report.exit_code.ok", exit_code(payload) == 0)
        checker.check(
            "report.shape",
            payload["kind"] == "wing-perf"
            and payload["shape"] == 1
            and list(payload)
            == [
                "kind",
                "shape",
                "base_ref",
                "head_ref",
                "base_sha",
                "head_sha",
                "rounds",
                "quick",
                "calibrate",
                "comparisons",
                "failures",
                "meta",
            ],
            str(list(payload)),
        )

        # ⑤ 报告渲染（report.py CLI，真子进程）
        ab_json = workdir / "ab.json"
        write_json(ab_json, payload)
        md_path = workdir / "comment.md"
        proc = _run(
            [
                sys.executable,
                str(HERE / "report.py"),
                "--json",
                str(ab_json),
                "--out-md",
                str(md_path),
                "--run-url",
                "https://example.invalid/runs/1",
                "--label",
                "selftest",
            ]
        )
        md = md_path.read_text(encoding="utf-8") if md_path.is_file() else ""
        checker.check(
            "report.cli", proc.returncode == 0, (proc.stderr or "").strip()[-300:]
        )
        checker.check("report.marker", md.startswith("<!-- wing-perf -->"))
        checker.check(
            "report.notes_visible",
            "未提交改动" in md and "> ⚠️" in md,
            "",
        )
        checker.check(
            "report.suite_notes_visible",
            "stub/base r1: stub note for base r1" in md
            and "其余" in md
            and "stub/head r3: stub note for head r3" in md,
            "",
        )
        checker.check(
            "report.conclusion",
            "**结论**" in md
            and "1 项回归" in md
            and "1 项关注" in md
            and "2 项改善" in md
            and "2 项持平" in md,
            md.splitlines()[3] if len(md.splitlines()) > 3 else "",
        )
        checker.check(
            "report.table",
            "| 指标 | base | head | Δ | 判定 |" in md
            and "`stub.worse.median_ms`" in md
            and "+40.0%" in md
            and "10 ms" in md,
            "",
        )
        checker.check(
            "report.order",
            md.index("stub.worse.median_ms")
            < md.index("stub.watch.median_ms")
            < md.index("stub.better.median_ms")
            < md.index("stub.steady.median_ms"),
        )
        checker.check(
            "report.footnote",
            "方法与口径" in md
            and "噪声 6.0% / 回归 20.0%" in md
            and "thresholds.json" in md
            and "https://example.invalid/runs/1" in md
            and "selftest" in md,
            "",
        )
        checker.check("report.stub_banner", "自测数据" in md)

        # ⑥ 多份 JSON 合并（03 的 report job 形态）
        merged_md_path = workdir / "merged.md"
        proc = _run(
            [
                sys.executable,
                str(HERE / "report.py"),
                "--json",
                str(ab_json),
                str(ab_json),
                "--out-md",
                str(merged_md_path),
            ]
        )
        merged_md = (
            merged_md_path.read_text(encoding="utf-8")
            if merged_md_path.is_file()
            else ""
        )
        checker.check(
            "report.merge",
            proc.returncode == 0 and merged_md.count("`stub.worse.median_ms`") == 2,
            f"rc={proc.returncode} count={merged_md.count('`stub.worse.median_ms`')}",
        )

        # ⑥b 不一致合并（03 的 report job 若拿到不同 sha / 档位的产物）：告警必须可见、表头不得说谎
        other_json = workdir / "ab-other.json"
        write_json(
            other_json,
            build_report(
                replace(
                    ctx, rounds=2, quick=True, base_sha="d" * 40, head_sha="e" * 40
                ),
                comparisons,
                [],
                [],
                2.0,
            ),
        )
        merged_bad = report_render.merge_reports(
            [read_json(ab_json), read_json(other_json)]
        )
        checker.check(
            "report.merge_flags",
            merged_bad["meta"]["sha_mismatch"] is True
            and merged_bad["meta"]["shape_mismatch"] is True
            and any("不可直接比较" in note for note in merged_bad["meta"]["notes"]),
            str(merged_bad["meta"]),
        )
        merged_bad_path = workdir / "merged-inconsistent.md"
        proc = _run(
            [
                sys.executable,
                str(HERE / "report.py"),
                "--json",
                str(ab_json),
                str(other_json),
                "--out-md",
                str(merged_bad_path),
            ]
        )
        merged_bad_md = (
            merged_bad_path.read_text(encoding="utf-8")
            if merged_bad_path.is_file()
            else ""
        )
        header_line = next(
            (line for line in merged_bad_md.splitlines() if line.startswith("`base` ")),
            "",
        )
        checker.check(
            "report.merge_inconsistent",
            proc.returncode == 0
            and "不可直接比较" in merged_bad_md
            and "sha 不一致" in header_line
            and "轮数 / 档位不一致" in header_line
            and "轮交错 A/B" not in merged_bad_md
            and "quick 档" not in merged_bad_md,
            f"rc={proc.returncode} header={header_line}",
        )

        # ⑥c suite 输出一致性守卫：suite/side/round 与请求不符 → Failure（不静默接受）
        raw_guard = workdir / "raw-guard"
        log = io.StringIO()
        with contextlib.redirect_stdout(log):
            _, failures_bad_side = orchestrate(
                suites=("stubside",),
                sides=sides,
                rounds=1,
                invoker=SubprocessInvoker(
                    script_dir=stub_dir,
                    side_json=side_json,
                    raw_dir=raw_guard,
                    quick=False,
                ),
                prepare=lambda suite: [],
            )
            _, failures_bad_round = orchestrate(
                suites=("stubround",),
                sides=sides,
                rounds=1,
                invoker=SubprocessInvoker(
                    script_dir=stub_dir,
                    side_json=side_json,
                    raw_dir=raw_guard,
                    quick=False,
                ),
                prepare=lambda suite: [],
            )
        checker.check(
            "guard.side_mismatch",
            len(failures_bad_side) == 2
            and all(
                "output mismatch" in failure.error and "side=" in failure.error
                for failure in failures_bad_side
            ),
            str([failure.error for failure in failures_bad_side]),
        )
        checker.check(
            "guard.round_mismatch",
            len(failures_bad_round) == 2
            and all(
                "output mismatch" in failure.error and "round=" in failure.error
                for failure in failures_bad_round
            ),
            str([failure.error for failure in failures_bad_round]),
        )

        # ⑦ 失败路径：suite 失败 → failures[] + 退出码非 0 + 其余测量继续
        raw_fail = workdir / "raw-fail"
        invoker_fail = SubprocessInvoker(
            script_dir=stub_dir, side_json=side_json, raw_dir=raw_fail, quick=False
        )
        log = io.StringIO()
        with contextlib.redirect_stdout(log):
            results_f, failures_f = orchestrate(
                suites=("stubfail", "stub"),
                sides=sides,
                rounds=2,
                invoker=invoker_fail,
                prepare=lambda suite: [],
            )
        order_f = _parse_order(log.getvalue())
        checker.check(
            "failure.order_continues",
            order_f
            == [
                ("stubfail", "base", 1),
                ("stubfail", "head", 1),
                ("stubfail", "base", 2),
                ("stubfail", "head", 2),
                ("stub", "base", 1),
                ("stub", "head", 1),
                ("stub", "base", 2),
                ("stub", "head", 2),
            ],
            str(order_f),
        )
        checker.check(
            "failure.recorded",
            len(failures_f) == 2
            and all(
                failure.suite == "stubfail"
                and failure.side == "head"
                and failure.round in (1, 2)
                and failure.exit_code == 1
                for failure in failures_f
            ),
            str([failure.to_json() for failure in failures_f]),
        )
        checker.check(
            "failure.reason",
            all("injected failure" in failure.error for failure in failures_f),
            str([failure.error for failure in failures_f]),
        )
        checker.check(
            "failure.continues",
            {result.suite for result in results_f} == {"stubfail", "stub"}
            and sum(1 for result in results_f if result.suite == "stub") == 4,
            f"{len(results_f)} results",
        )
        comparisons_f, notes_f = build_comparisons(results_f, thresholds)
        by_f = {str(entry["metric"]): entry for entry in comparisons_f}
        checker.check(
            "failure.n_a_rows",
            by_f["stubfail.steady.median_ms"]["verdict"] == "n/a"
            and by_f["stubfail.steady.median_ms"]["delta_pct"] is None
            and by_f["stubfail.steady.median_ms"].get("note") == "head 侧无数据",
            str(by_f["stubfail.steady.median_ms"]),
        )
        checker.check(
            "failure.healthy_compared",
            by_f["stub.worse.median_ms"]["verdict"] == "regression",
        )
        payload_f = build_report(
            replace(ctx, suites=("stubfail", "stub"), rounds=2),
            comparisons_f,
            failures_f,
            notes_f,
            1.0,
        )
        checker.check("failure.exit_code", exit_code(payload_f) == 1)
        md_f = report_render.render_comment(payload_f)
        checker.check(
            "report.failures_shown",
            "stubfail" in md_f and "套件失败" in md_f and "injected failure" in md_f,
            "",
        )
        checker.check(
            "report.unjudged_shown",
            "未判定：" in md_f
            and "`stubfail.steady.median_ms`" in md_f
            and "head 侧无数据" in md_f
            and "🚫 不判定" in md_f,
            "",
        )
        # 失败现场（log_tail）同样不能把 ANSI 带进评论：cargo 的彩色 stderr 会落进日志尾部。
        ansi_failure = Failure(
            suite="rust",
            side="base",
            round=1,
            error="suite exited 101",
            exit_code=101,
            log_tail="\x1b[1m\x1b[91merror\x1b[0m: could not compile `wing`",
        )
        md_fail_ansi = report_render.render_comment(
            build_report(ctx, [], [ansi_failure], [], 1.0)
        )
        checker.check(
            "report.failure_tail_ansi_stripped",
            "\x1b" not in md_fail_ansi
            and "error: could not compile `wing`" in md_fail_ansi
            and "r1" in md_fail_ansi,
            repr(md_fail_ansi[-300:]),
        )

        # ⑧ prepare 失败 → 跳过该 suite、其余继续
        def prepare_broken(suite: str) -> list[Failure]:
            if suite == "stubfail":
                return [
                    Failure(
                        suite=suite,
                        side="base",
                        round=None,
                        error="prepare `cargo bench --no-run -p wing` exited 101",
                        exit_code=101,
                    )
                ]
            return []

        raw_prep = workdir / "raw-prepare"
        log = io.StringIO()
        with contextlib.redirect_stdout(log):
            results_p, failures_p = orchestrate(
                suites=("stubfail", "stub"),
                sides=sides,
                rounds=1,
                invoker=SubprocessInvoker(
                    script_dir=stub_dir,
                    side_json=side_json,
                    raw_dir=raw_prep,
                    quick=False,
                ),
                prepare=prepare_broken,
            )
        order_p = _parse_order(log.getvalue())
        checker.check(
            "prepare.failure_skips_suite",
            len(failures_p) == 1
            and all(result.suite == "stub" for result in results_p)
            and order_p == [("stub", "base", 1), ("stub", "head", 1)],
            f"{len(failures_p)} failures / {order_p}",
        )
        checker.check(
            "prepare.exit_code",
            exit_code(build_report(replace(ctx, rounds=1), [], failures_p, [], 1.0))
            == 1,
        )

        # ⑨ 边界：单轮 / --quick 透传
        raw_quick = workdir / "raw-quick"
        log = io.StringIO()
        with contextlib.redirect_stdout(log):
            results_q, failures_q = orchestrate(
                suites=("stub",),
                sides=sides,
                rounds=1,
                invoker=SubprocessInvoker(
                    script_dir=stub_dir,
                    side_json=side_json,
                    raw_dir=raw_quick,
                    quick=True,
                ),
                prepare=lambda suite: [],
            )
        checker.check(
            "quick.flag_propagated",
            len(results_q) == 2
            and not failures_q
            and all(
                result.meta.get("config", {}).get("quick") is True
                for result in results_q
            ),
            str([result.meta.get("config") for result in results_q]),
        )
        comparisons_q, _ = build_comparisons(results_q, thresholds)
        by_q = {str(entry["metric"]): entry for entry in comparisons_q}
        checker.check(
            "single_round.lists",
            all(
                len(entry["base"]) == 1 and len(entry["head"]) == 1
                for entry in comparisons_q
            ),
        )
        checker.check(
            "single_round.verdict",
            by_q["stub.worse.median_ms"]["head"] == [13.0]
            and by_q["stub.worse.median_ms"]["verdict"] == "regression",
            str(by_q["stub.worse.median_ms"]),
        )
        payload_q = build_report(replace(ctx, rounds=1), comparisons_q, [], [], 1.0)
        checker.check(
            "single_round.note",
            any("rounds=1" in note for note in payload_q["meta"]["notes"]),
            str(payload_q["meta"]["notes"]),
        )

        # ⑩ 边界：base 中位数为 0 / 指标缺失（合成数据）
        synth = [
            _synth(
                "stub",
                "base",
                1,
                {"stub.zero.median_ms": 0.0, "stub.shared.median_ms": 5.0},
            ),
            _synth(
                "stub",
                "head",
                1,
                {"stub.zero.median_ms": 4.0, "stub.shared.median_ms": 5.0},
            ),
            _synth("stub", "head", 1, {"stub.head_only.median_ms": 3.0}),
        ]
        comparisons_e, _ = build_comparisons(synth, thresholds)
        by_e = {str(entry["metric"]): entry for entry in comparisons_e}
        checker.check(
            "edge.base_zero",
            by_e["stub.zero.median_ms"]["verdict"] == "n/a"
            and by_e["stub.zero.median_ms"]["delta_pct"] is None
            and "0" in str(by_e["stub.zero.median_ms"].get("note")),
            str(by_e["stub.zero.median_ms"]),
        )
        checker.check(
            "edge.missing_metric",
            by_e["stub.head_only.median_ms"]["verdict"] == "n/a"
            and "无数据" in str(by_e["stub.head_only.median_ms"].get("note")),
            str(by_e["stub.head_only.median_ms"]),
        )
        checker.check(
            "edge.comparable_survives",
            by_e["stub.shared.median_ms"]["verdict"] == "flat",
        )
        # 负基准与零基准同类：百分比在有符号量上没有方向含义（B 级原文的触发条件）。
        negative = [
            _synth("stub", "base", 1, {"stub.neg.median_ms": -5.0}),
            _synth("stub", "head", 1, {"stub.neg.median_ms": 20.0}),
        ]
        comparisons_n, _ = build_comparisons(negative, thresholds)
        by_n = {str(entry["metric"]): entry for entry in comparisons_n}
        checker.check(
            "edge.base_negative",
            by_n["stub.neg.median_ms"]["verdict"] == "n/a"
            and by_n["stub.neg.median_ms"]["delta_pct"] is None
            and "基准非正" in str(by_n["stub.neg.median_ms"].get("note")),
            str(by_n["stub.neg.median_ms"]),
        )

        # ⑩b 信息项（info_only）：数值照给、不判档；报告里不跟"改善"混在一起
        info = [
            _synth(
                "tui",
                "base",
                1,
                {"tui.display.coverage_ratio": 0.6, "tui.display.lag_p50_us": 24000.0},
            ),
            _synth(
                "tui",
                "head",
                1,
                {"tui.display.coverage_ratio": 0.5, "tui.display.lag_p50_us": 26400.0},
            ),
        ]
        comparisons_i, _ = build_comparisons(info, thresholds)
        by_i = {str(entry["metric"]): entry for entry in comparisons_i}
        coverage = by_i["tui.display.coverage_ratio"]
        # 覆盖率 -16.7% 在 lower-better 下会被判成 improved —— 那正是要禁止的结论。
        checker.check(
            "info_only.verdict",
            coverage["verdict"] == "n/a"
            and abs(float(coverage["delta_pct"]) + 16.667) < 1e-3
            and "信息项" in str(coverage.get("note")),
            str(coverage),
        )
        checker.check(
            "info_only.judged_neighbour",
            by_i["tui.display.lag_p50_us"]["verdict"] == "flat"
            and by_i["tui.display.lag_p50_us"]["noise_pct"] == 15.0,
            str(by_i["tui.display.lag_p50_us"]),
        )
        md_i = report_render.render_comment(
            build_report(ctx, comparisons_i, [], [], 1.0)
        )
        checker.check(
            "info_only.report",
            "`tui.display.coverage_ratio`" in md_i
            and "🚫 不判定" in md_i
            and "-16.7%" in md_i
            and "未判定：`tui.display.coverage_ratio`：测量质量信息项" in md_i,
            "",
        )
        checker.check(
            "info_only.counted",
            "1 项未判定" in md_i and "项改善" not in md_i,
            md_i.splitlines()[3] if len(md_i.splitlines()) > 3 else "",
        )

        # ⑩c 有符号零中心量（trend）：判档没有方向意义（base<0 时 Δ% 符号还会翻转）
        trend = [
            _synth("tui", "base", 1, {"tui.display.trend_us": 20000.0}),
            _synth("tui", "head", 1, {"tui.display.trend_us": 10000.0}),
        ]
        comparisons_t, _ = build_comparisons(trend, thresholds)
        by_t = {str(entry["metric"]): entry for entry in comparisons_t}
        checker.check(
            "info_only.signed_trend",
            by_t["tui.display.trend_us"]["verdict"] == "n/a"
            and abs(float(by_t["tui.display.trend_us"]["delta_pct"]) + 50.0) < 1e-6
            and "信息项" in str(by_t["tui.display.trend_us"].get("note")),
            str(by_t["tui.display.trend_us"]),
        )
        # 负基准时守卫先接管：连 Δ% 都不给（否则 -50% 会被读成"改善"）。
        trend_neg = [
            _synth("tui", "base", 1, {"tui.display.trend_us": -20000.0}),
            _synth("tui", "head", 1, {"tui.display.trend_us": 20000.0}),
        ]
        comparisons_tn, _ = build_comparisons(trend_neg, thresholds)
        by_tn = {str(entry["metric"]): entry for entry in comparisons_tn}
        checker.check(
            "info_only.signed_trend_negative_base",
            by_tn["tui.display.trend_us"]["verdict"] == "n/a"
            and by_tn["tui.display.trend_us"]["delta_pct"] is None
            and "基准非正" in str(by_tn["tui.display.trend_us"].get("note")),
            str(by_tn["tui.display.trend_us"]),
        )

        # ⑩d 质量信号消费：载荷指纹（两侧不一致 → 该 suite 全行 n/a；单侧有 → 跳过）
        same = {"200": {"fnv1a": "aa"}, "1000": {"fnv1a": "bb"}}
        checked_same = [
            _synth(
                "rust",
                "base",
                1,
                {"rust.a.median_ns": 10.0},
                config={"payload_fingerprints": same},
            ),
            _synth(
                "rust",
                "head",
                1,
                {"rust.a.median_ns": 9.0},
                config={"payload_fingerprints": dict(same)},
            ),
        ]
        checker.check(
            "fingerprint.same_side_values",
            not fingerprint_mismatches(checked_same),
            str(fingerprint_mismatches(checked_same)),
        )
        one_sided = [
            _synth("rust", "base", 1, {"rust.a.median_ns": 10.0}),
            _synth(
                "rust",
                "head",
                1,
                {"rust.a.median_ns": 9.0},
                config={"payload_fingerprints": same},
            ),
        ]
        checker.check(
            "fingerprint.single_side_skipped",
            not fingerprint_mismatches(one_sided),
            str(fingerprint_mismatches(one_sided)),
        )
        comparisons_one, notes_one = build_comparisons(one_sided, thresholds)
        checker.check(
            "fingerprint.single_side_judged",
            len(comparisons_one) == 1
            and comparisons_one[0]["verdict"] == "improved"
            and not notes_one,
            str(comparisons_one),
        )
        differing = [
            _synth(
                "rust",
                "base",
                1,
                {"rust.a.median_ns": 10.0},
                config={"payload_fingerprints": same},
            ),
            _synth(
                "rust",
                "head",
                1,
                {"rust.a.median_ns": 9.0},
                config={
                    "payload_fingerprints": {
                        "200": {"fnv1a": "zz"},
                        "1000": {"fnv1a": "bb"},
                    }
                },
            ),
        ]
        comparisons_d, notes_d = build_comparisons(differing, thresholds)
        checker.check(
            "fingerprint.mismatch_n_a",
            len(comparisons_d) == 1
            and comparisons_d[0]["verdict"] == "n/a"
            and "载荷指纹不一致" in str(comparisons_d[0].get("note"))
            and len(comparisons_d[0]["base"]) == 1,
            str(comparisons_d[0]),
        )
        checker.check(
            "fingerprint.mismatch_note",
            any(
                "载荷指纹不一致" in note and "n=200" in note and "aa" in note
                for note in notes_d
            ),
            str(notes_d),
        )

        # ⑩e 质量信号消费：criterion CI 宽度 > 10% → 提示 note（不判档）
        wide = [
            _synth(
                "rust",
                "base",
                1,
                {"rust.a.median_ns": 10.0, "rust.b.median_ns": 2.0},
                config={
                    "ci_rel_pct": {"rust.a.median_ns": 3.5, "rust.b.median_ns": 22.5}
                },
            ),
            _synth(
                "rust",
                "head",
                1,
                {"rust.a.median_ns": 8.0, "rust.b.median_ns": 1.95},
                config={
                    "ci_rel_pct": {"rust.a.median_ns": 4.0, "rust.b.median_ns": 11.0}
                },
            ),
        ]
        comparisons_w, notes_w = build_comparisons(wide, thresholds)
        by_w = {str(entry["metric"]): entry for entry in comparisons_w}
        checker.check(
            "ci_width.note",
            len(notes_w) == 1
            and "置信区间偏宽" in notes_w[0]
            and "rust.b.median_ns 22.5%" in notes_w[0]
            and "rust.a.median_ns" not in notes_w[0],
            str(notes_w),
        )
        checker.check(
            "ci_width.verdict_untouched",
            by_w["rust.b.median_ns"]["verdict"] == "flat"
            and abs(float(by_w["rust.b.median_ns"]["delta_pct"]) + 2.5) < 1e-6
            and by_w["rust.a.median_ns"]["verdict"] == "improved",
            str(by_w),
        )
        checker.check(
            "ci_width.below_threshold_silent",
            not ci_width_notes(
                [
                    _synth(
                        "rust",
                        "base",
                        1,
                        {"rust.a.median_ns": 10.0},
                        config={"ci_rel_pct": {"rust.a.median_ns": 10.0}},
                    )
                ]
            ),
            str(
                ci_width_notes(
                    [
                        _synth(
                            "rust",
                            "base",
                            1,
                            {"rust.a.median_ns": 10.0},
                            config={"ci_rel_pct": {"rust.a.median_ns": 10.0}},
                        )
                    ]
                )
            ),
        )

        # ⑪ 校准语义：两侧同 rev。用 HEAD~1（≠ HEAD）构造——实现里删掉"校准钉住 head"就会红。
        try:
            revs_head = resolve_revs(REPO_ROOT, "HEAD", calibrate=False)
            revs_cal = resolve_revs(REPO_ROOT, "HEAD~1", calibrate=True)
            revs_off = resolve_revs(REPO_ROOT, "HEAD~1", calibrate=False)
        except CommandError as exc:
            checker.skip("calibrate.revs", f"no git checkout with HEAD~1: {exc}")
        else:
            checker.check(
                "calibrate.revs",
                revs_cal.base_sha == revs_cal.head_sha == revs_head.head_sha
                and revs_cal.base_ref == "HEAD~1"
                and revs_off.base_sha != revs_off.head_sha,
                f"cal={revs_cal.base_sha[:8]} head={revs_cal.head_sha[:8]} "
                f"off={revs_off.base_sha[:8]}",
            )
            payload_cal = build_report(
                replace(
                    ctx,
                    calibrate=True,
                    base_sha=revs_cal.base_sha,
                    head_sha=revs_cal.head_sha,
                ),
                [],
                [],
                [],
                0.0,
            )
            checker.check(
                "calibrate.note",
                any("calibrate" in note for note in payload_cal["meta"]["notes"]),
                str(payload_cal["meta"]["notes"]),
            )

        # ⑪b CLI 端到端（隔离布局：临时 git 仓库 + harness 副本 + stub 的 suite_rust.py）：
        #     `--calibrate` 是否真的透传到两侧 rev、材料化的 base worktree 是否落在同一 rev。
        try:
            scratch_repo = workdir / "cli-repo"
            prev_sha, scratch_head = _init_scratch_repo(scratch_repo)
        except CommandError as exc:
            checker.skip("calibrate.cli", f"cannot create a scratch git repo: {exc}")
        else:
            harness = _copy_harness(workdir / "cli-harness")
            _write_stub(harness, "ok", suite="rust")
            # 路径故意带空格：顺带钉住"清理提示可复制执行"（shlex.quote）与含空格 workdir。
            cal_workdir = workdir / "cli-cal dir"
            cal_json = workdir / "cli-cal.json"
            proc = _run(
                [
                    sys.executable,
                    str(harness / "ab.py"),
                    "--repo",
                    str(scratch_repo),
                    "--base-ref",
                    "HEAD~1",
                    "--calibrate",
                    "--suites",
                    "rust",
                    "--rounds",
                    "1",
                    "--no-prepare",
                    "--workdir",
                    str(cal_workdir),
                    "--out-json",
                    str(cal_json),
                    "--out-md",
                    str(workdir / "cli-cal.md"),
                ]
            )
            cal = _read_or_empty(cal_json)
            checker.check(
                "calibrate.cli.same_rev",
                proc.returncode == 0
                and cal.get("calibrate") is True
                and cal.get("base_sha") == cal.get("head_sha") == scratch_head
                and cal.get("base_ref") == "HEAD~1",
                f"rc={proc.returncode} base={str(cal.get('base_sha'))[:8]} "
                f"head={str(cal.get('head_sha'))[:8]} {proc.stderr.strip()[-200:]}",
            )
            checker.check(
                "calibrate.cli.worktree",
                _rev_of(cal_workdir / "base") == scratch_head,
                _rev_of(cal_workdir / "base")[:12],
            )
            order_cli = _parse_order(proc.stdout)
            checker.check(
                "cli.interleaved",
                order_cli == [("rust", "base", 1), ("rust", "head", 1)],
                str(order_cli),
            )
            quoted = shlex.quote(
                str(cal_workdir.resolve())
            )  # run_ab 打印 resolve() 后的路径
            checker.check(
                "cli.space_path_hint",
                quoted in proc.stdout,  # 去掉 shlex.quote 就会红
                f"quoted={quoted} in stdout={quoted in proc.stdout}",
            )
            checker.check(
                "calibrate.cli.notes",
                any(
                    "calibrate" in note
                    for note in (cal.get("meta") or {}).get("notes", [])
                ),
                str((cal.get("meta") or {}).get("notes")),
            )
            checker.check(
                "notes.cli_propagation",
                [
                    note
                    for note in (cal.get("meta") or {}).get("notes", [])
                    if note.startswith("rust/")
                ]
                == [
                    "rust/base r1: stub note for base r1",
                    "rust/head r1: stub note for head r1",
                ],
                str((cal.get("meta") or {}).get("notes")),
            )
            checker.check(
                "cli.success_path",
                proc.returncode == 0
                and len(cal.get("comparisons") or []) == 6
                and not cal.get("failures"),
                f"{len(cal.get('comparisons') or [])} comparisons / "
                f"{len(cal.get('failures') or [])} failures",
            )
            off_workdir = workdir / "cli-off"
            off_json = workdir / "cli-off.json"
            proc = _run(
                [
                    sys.executable,
                    str(harness / "ab.py"),
                    "--repo",
                    str(scratch_repo),
                    "--base-ref",
                    "HEAD~1",
                    "--suites",
                    "rust",
                    "--rounds",
                    "1",
                    "--no-prepare",
                    "--workdir",
                    str(off_workdir),
                    "--out-json",
                    str(off_json),
                    "--out-md",
                    str(workdir / "cli-off.md"),
                ]
            )
            off = _read_or_empty(off_json)
            checker.check(
                "calibrate.cli.off",
                proc.returncode == 0
                and off.get("calibrate") is False
                and off.get("base_sha") == prev_sha
                and off.get("head_sha") == scratch_head,
                f"rc={proc.returncode} base={str(off.get('base_sha'))[:8]} "
                f"prev={prev_sha[:8]} {proc.stderr.strip()[-200:]}",
            )
            checker.check(
                "calibrate.cli.worktree_off",
                _rev_of(off_workdir / "base") == prev_sha,
                _rev_of(off_workdir / "base")[:12],
            )

        # ⑫ CLI 契约：--help 参数齐全 / 未知 suite / 缺 --base-ref
        proc = _run([sys.executable, str(HERE / "ab.py"), "--help"])
        flags = (
            "--repo",
            "--base-ref",
            "--workdir",
            "--suites",
            "--rounds",
            "--quick",
            "--prepare",
            "--no-prepare",
            "--calibrate",
            "--out-json",
            "--out-md",
            "--selftest",
        )
        missing = [flag for flag in flags if flag not in proc.stdout]
        checker.check(
            "cli.help_flags",
            proc.returncode == 0 and not missing,
            f"missing {missing}",
        )
        proc = _run(
            [
                sys.executable,
                str(HERE / "ab.py"),
                "--suites",
                "bogus",
                "--base-ref",
                "HEAD",
                "--workdir",
                str(workdir / "bogus"),
            ]
        )
        text = proc.stdout + proc.stderr
        checker.check(
            "cli.unknown_suite",
            proc.returncode != 0 and "bogus" in text,
            f"rc={proc.returncode} {text.strip()[-200:]}",
        )
        proc = _run(
            [sys.executable, str(HERE / "ab.py"), "--workdir", str(workdir / "noref")]
        )
        text = proc.stdout + proc.stderr
        checker.check(
            "cli.missing_base_ref",
            proc.returncode != 0 and "--base-ref" in text,
            f"rc={proc.returncode} {text.strip()[-200:]}",
        )
        proc = _run(
            [
                sys.executable,
                str(HERE / "report.py"),
                "--json",
                str(workdir / "does-not-exist.json"),
                "--out-md",
                str(workdir / "never.md"),
            ]
        )
        text = proc.stdout + proc.stderr
        checker.check(
            "cli.report_input_error",
            proc.returncode == 2 and "cannot read" in text and "Traceback" not in text,
            f"rc={proc.returncode} {text.strip()[-200:]}",
        )

    return checker.summary()


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    if args.selftest:
        return run_selftest()
    try:
        return run_ab(args)
    except CommandError as exc:
        print(f"[perf] error: {exc}", file=sys.stderr, flush=True)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
