#!/usr/bin/env python3
"""TUI 端到端显示延迟 suite（perf-ci 04）：复用 ``scripts/demo/latency.py`` 的核心测量。

契约（§2 CLI / §3 输出 JSON）见 perf-ci 任务书；口径（指标映射、档位、环境隔离与清理、
取舍理由）见 04 步骤的 ``design.md``。一句话：真网关（side 的 wing-gateway）+ 真 TUI
（side 的 release wing），假 Provider 按 3000 tok/s 灌语料并每 400 帧插一行
``⟦M#####⟧`` 标记，``lag = 标记出现在 pty 字节流 − Provider 发射``。

指标只取 3000 tok/s 档（``--quick`` 单次 6s；默认档额外跑一次 30000 tok/s，仅信息性、
不进指标）；命中率低于 0.6 时照常出指标、但在 meta.notes 标注"低置信"。一次测量若
**一个标记都没采到**（TUI 没在窗口里画出首屏，冷启动下首帧可花 4s+），最多重跑
``MAX_ATTEMPTS`` 次 —— 那是 rig 的启动竞态、不是性能信号；重试仍失败才算 suite 失败。

    uv run python scripts/perf/suite_tui.py \
        --side target/perf/sides/head.json --out /tmp/tui.json --round 1 --quick

环境纪律（本次测量的所有子进程共享）：``WING_BIN`` / ``WING_GATEWAY_BIN`` 钉到 side；
``DEMO_ROOT`` / ``DEMO_WING_HOME`` / ``DEMO_WORKSPACE`` 落在本次 (side, round) 独立的
scratch（成功即删、失败保留供排查）；清空代理环境变量并显式 ``NO_PROXY``（loopback
必须直连）。rig（``scripts/demo/``）取自 head 侧，两侧共用。
"""

from __future__ import annotations

import argparse
import math
import os
import shutil
import sys
import traceback
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
DEMO = REPO / "scripts" / "demo"  # rig：两侧共用 head 版本的 measurement stack
if str(DEMO) not in sys.path:
    sys.path.insert(0, str(DEMO))

from common import (  # noqa: E402
    CommandError,
    Side,
    SuiteOutput,
    finite,
    median,
)

import latency  # noqa: E402  (scripts/demo/latency.py 的核心测量函数)

#: 主速率（进指标）与信息性速率（只落 meta，不参与判定）。
RATE_MAIN = 3000
RATE_INFO = 30000
#: 档位（04 任务书冻结）：quick = 3000 × 6s × 1；默认（full）= 3000 × 10s × 3 + 30000 × 10s × 1。
QUICK_SECONDS = 6.0
FULL_SECONDS = 10.0
QUICK_REPEATS = 1
FULL_REPEATS = 3
MARKER_EVERY = 400
#: 首击后这么久 provider 还是零帧（emit log 为空）→ 补发同一句 prompt（见 design.md
#: D7「启动竞态」：TUI 启动期的终端查询阶段会冲掉先到的按键）。只给本套件开；CLI 的
#: 默认行为不变（`latency.run_rate` 的 `resend_after=None`）。
RESEND_AFTER = 0.7
#: 命中率低于它 → meta.notes 标注"低置信"（指标照常输出；与 latency.py 表格里的
#: MIN_COVERAGE=0.4「样本不足」是两件事：一个管套件置信，一个管人类的终端读数）。
LOW_COVERAGE = 0.6
#: 一次测量"没采到任何标记"= TUI 没在这个窗口里画出首屏（pty fork → 首帧在冷启动时
#: 可花 4s+，见 design.md D7），属 rig 启动竞态而不是性能信号：同一次测量最多重跑这么多
#: 次；仍然没有标记才作废（quick 档没有备用测量 → suite 失败）。
MAX_ATTEMPTS = 2

#: row 字段 → (metric id, 换算系数)：秒 → µs；cpu 百分数 → ratio。
METRICS: tuple[tuple[str, str, float], ...] = (
    ("p50", "tui.display.lag_p50_us", 1e6),
    ("p99", "tui.display.lag_p99_us", 1e6),
    ("max", "tui.display.lag_max_us", 1e6),
    ("coverage", "tui.display.coverage_ratio", 1.0),
    ("trend", "tui.display.trend_us", 1e6),
    ("cpu", "tui.tui_cpu_ratio", 1e-2),
)

#: 必须从本次 scratch 走直连的环境变量：代理会把 loopback 也送进代理。
PROXY_KEYS: tuple[str, ...] = (
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="suite_tui.py",
        description="end-to-end TUI display-latency suite (perf-ci contract §2/§3)",
    )
    parser.add_argument("--side", required=True, help="side 描述符 JSON（契约 §1）")
    parser.add_argument("--out", required=True, help="suite 输出 JSON（契约 §3）")
    parser.add_argument(
        "--round", type=int, required=True, help="轮号（ab.py 交错轮次）"
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="CI 档：3000 tok/s × 6s × 1（默认档为 3000×10s×3 + 30000×10s×1）",
    )
    return parser.parse_args(argv)


def check_side(side: Side) -> str | None:
    """side 描述符指向的可执行文件是否存在；返回 None = 就绪，否则是给 failures[] 的原因。"""
    for label, path in (("wing", side.wing_bin), ("wing-gateway", side.gateway_bin)):
        if not path.is_file():
            return (
                f"side {side.name}: missing {label} binary {path} — run with --prepare "
                "(cargo build --release -p wing / uv sync --frozen)"
            )
    if not side.python.is_file():
        return (
            f"side {side.name}: missing venv python {side.python} — run without "
            "--no-prepare (uv sync --frozen) or create it first"
        )
    return None


def make_scratch(side: Side, round_no: int) -> Path:
    """本次 (side, round) 的独立 scratch：``home/`` 是网关的 $WING_HOME，``ws/`` 是工作区。"""
    scratch = side.wing_home / f"tui-r{round_no}"
    shutil.rmtree(scratch, ignore_errors=True)
    (scratch / "home").mkdir(parents=True)
    (scratch / "ws").mkdir(parents=True)
    return scratch


def prepare_environment(side: Side, scratch: Path) -> None:
    """把 side 二进制与隔离 scratch 钉进 ``os.environ``：本次测量的子进程都继承它。"""
    os.environ["WING_BIN"] = str(side.wing_bin)
    os.environ["WING_GATEWAY_BIN"] = str(side.gateway_bin)
    os.environ["DEMO_ROOT"] = str(scratch)
    os.environ["DEMO_WING_HOME"] = str(scratch / "home")
    os.environ["DEMO_WORKSPACE"] = str(scratch / "ws")
    for key in PROXY_KEYS:
        os.environ.pop(key, None)
    os.environ["NO_PROXY"] = "127.0.0.1,localhost"
    os.environ["no_proxy"] = "127.0.0.1,localhost"


def fmt_ms(seconds: float) -> str:
    """row 里的秒 → 日志用的 ``NN.Nms``；非有限值显示 ``n/a``。"""
    if not math.isfinite(seconds):
        return "n/a"
    return f"{seconds * 1000:.1f}ms"


def measure_rate(
    rate: int, seconds: float, repeats: int, scratch: Path, out: SuiteOutput
) -> list[dict[str, float]]:
    """跑 ``repeats`` 次 ``rate`` tok/s 的测量，返回成功采到标记的各次 row。

    没采到标记的一次（TUI 没在窗口内画出首屏）最多重跑 ``MAX_ATTEMPTS`` 次：这是 rig 的
    启动竞态，不是性能信号，重试不掩盖任何回归；重试与跳过都记进 ``out`` 的 notes。
    emit log 保留在 scratch 里（``keep=True``）：成功时随 scratch 一起删、失败时正是
    排查"标记为什么没被看到"的第一手材料。
    """
    rows: list[dict[str, float]] = []
    for index in range(1, repeats + 1):
        row: dict[str, float] | None = None
        attempts = 0
        for attempt in range(1, MAX_ATTEMPTS + 1):
            attempts = attempt
            emit = scratch / f"emit-{rate}-r{index}-a{attempt}.jsonl"
            candidate = latency.run_rate(
                rate,
                seconds,
                MARKER_EVERY,
                True,
                emit_path=emit,
                resend_after=RESEND_AFTER,
            )
            resends = int(candidate.get("resends", 0))
            print(
                f"[suite_tui] {rate} tok/s × {seconds:g}s r{index}/a{attempt}: coverage "
                f"{candidate['coverage'] * 100:.0f}% markers {int(candidate['markers'])} "
                f"missing {int(candidate['missing'])} p50 {fmt_ms(candidate['p50'])} "
                f"p99 {fmt_ms(candidate['p99'])} max {fmt_ms(candidate['max'])}"
                + (f" (resends {resends})" if resends else ""),
                flush=True,
            )
            if resends:
                out.note(
                    f"{rate} tok/s 第 {index} 次测量补发了 {resends} 次 prompt"
                    "（首击被 TUI 启动期的终端初始化冲掉，emit log 为空才补）"
                )
            if int(candidate["markers"]) > 0:
                row = candidate
                break
        if row is None:
            out.note(
                f"{rate} tok/s 第 {index} 次测量 {MAX_ATTEMPTS} 次尝试都没采到标记"
                "（TUI 未在窗口内画出首屏），该次不计入指标"
            )
        else:
            if attempts > 1:
                out.note(
                    f"{rate} tok/s 第 {index} 次测量在第 {attempts} 次尝试才采到标记"
                    "（rig 启动竞态，已重试）"
                )
            rows.append(row)
    return rows


def row_values(
    rows: Sequence[Mapping[str, float]], key: str, scale: float
) -> list[float]:
    """各 repeat 的 ``row[key]``（丢非有限值）乘上换算系数。"""
    return [value * scale for value in finite(row.get(key) for row in rows)]


def row_json(row: Mapping[str, float]) -> dict[str, Any]:
    """row（秒口径）→ 报告友好的 JSON：时间用 µs、cpu 用 ratio、非有限值 None。"""

    def number(value: float) -> float | None:
        return round(value, 3) if math.isfinite(value) else None

    return {
        "rate": int(row["rate"]),
        "coverage": number(row["coverage"]),
        "cpu_ratio": number(row["cpu"] / 100.0),
        "markers": int(row["markers"]),
        "missing": int(row["missing"]),
        "resends": int(row.get("resends", 0)),
        "min_us": number(row["min"] * 1e6),
        "p50_us": number(row["p50"] * 1e6),
        "p99_us": number(row["p99"] * 1e6),
        "max_us": number(row["max"] * 1e6),
        "trend_us": number(row["trend"] * 1e6),
    }


def fill_output(
    out: SuiteOutput,
    main_rows: Sequence[Mapping[str, float]],
    info_rows: Sequence[Mapping[str, float]],
    *,
    seconds: float,
    quick: bool,
) -> None:
    """把测量结果灌进 suite 输出：指标（代表值 = 各 repeat 中位数）+ 配置 + 置信标注。"""
    for key, metric_id, scale in METRICS:
        values = row_values(main_rows, key, scale)
        if not values:
            out.note(f"{metric_id}: 各次测量都没有有限数值，未输出该指标")
            continue
        out.metric(metric_id, round(median(values), 6), *(round(v, 6) for v in values))

    coverage = row_values(main_rows, "coverage", 1.0)
    if coverage and min(coverage) < LOW_COVERAGE:
        out.note(
            f"低置信：命中率 {min(coverage):.2f} < {LOW_COVERAGE}"
            "（多数标记在两帧之间被滚过，p50/p99 只代表能被逐帧抽到的那部分）"
        )

    out.setting("profile", "quick" if quick else "full")
    out.setting("rate_tps", RATE_MAIN)
    out.setting("seconds", seconds)
    out.setting("marker_every", MARKER_EVERY)
    out.setting("repeats", len(main_rows))
    out.setting("markers", sum(int(row["markers"]) for row in main_rows))
    out.setting("missing", sum(int(row["missing"]) for row in main_rows))
    out.setting("runs", [row_json(row) for row in main_rows])
    if info_rows:
        out.setting(
            "informational",
            {
                "rate_tps": RATE_INFO,
                "seconds": FULL_SECONDS,
                "runs": [row_json(row) for row in info_rows],
            },
        )
        out.note(
            f"{RATE_INFO} tok/s 档仅信息性（不进指标、不参与判定），数字见 "
            "meta.config.informational"
        )


def run(args: argparse.Namespace) -> int:
    side = Side.load(Path(args.side))
    out_path = Path(args.out)
    out = SuiteOutput(suite="tui", side=side.name, round=args.round, quick=args.quick)

    problem = check_side(side)
    if problem:
        print(f"[suite_tui] error: {problem}", file=sys.stderr, flush=True)
        out.write(out_path, ok=False, error=problem)
        return 1

    quick = bool(args.quick)
    seconds = QUICK_SECONDS if quick else FULL_SECONDS
    repeats = QUICK_REPEATS if quick else FULL_REPEATS
    scratch = make_scratch(side, args.round)
    prepare_environment(side, scratch)
    print(
        f"[suite_tui] side={side.name} r{args.round} profile="
        f"{'quick' if quick else 'full'} rate={RATE_MAIN} seconds={seconds:g} "
        f"repeats={repeats} scratch={scratch}",
        flush=True,
    )
    failures: str | None = None
    try:
        main_rows = measure_rate(RATE_MAIN, seconds, repeats, scratch, out)
        if not main_rows:
            failures = (
                f"no markers measured at {RATE_MAIN} tok/s in {repeats}×{MAX_ATTEMPTS} "
                "attempts: the TUI never streamed a turn (see the emit logs in the "
                "kept scratch)"
            )
            print(f"[suite_tui] error: {failures}", file=sys.stderr, flush=True)
            out.write(out_path, ok=False, error=failures)
            return 1
        info_rows: list[dict[str, float]] = []
        if not quick:
            try:
                info_rows = measure_rate(RATE_INFO, seconds, 1, scratch, out)
            except SystemExit as exc:
                # 信息性档没跑成不该丢主档指标；照实记一条 note。
                out.note(f"{RATE_INFO} tok/s 档未跑成（仅信息性）：{exc}")
                print(f"[suite_tui] info tier skipped: {exc}", file=sys.stderr)
        fill_output(out, main_rows, info_rows, seconds=seconds, quick=quick)
        out.write(out_path)
        return 0
    except SystemExit as exc:
        failures = f"measurement aborted: {exc}"
        print(f"[suite_tui] error: {failures}", file=sys.stderr, flush=True)
        out.write(out_path, ok=False, error=failures)
        return 1
    except Exception as exc:
        # 任何意外都要变成可解析的失败（ok=false + 原因），而不是裸 traceback 之外的静默。
        failures = f"{type(exc).__name__}: {exc}"
        traceback.print_exc()
        out.write(out_path, ok=False, error=failures)
        return 1
    finally:
        if failures is None:
            shutil.rmtree(scratch, ignore_errors=True)
        else:
            print(f"[suite_tui] scratch kept for debugging: {scratch}", flush=True)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        return run(args)
    except (ValueError, CommandError) as exc:
        print(f"[suite_tui] error: {exc}", file=sys.stderr, flush=True)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
