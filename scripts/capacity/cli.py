#!/usr/bin/env python3
"""命令行面：参数解析 + 校验（``--scenario`` / ``--levels`` 等冻结名字的唯一来源）。

冻结的 CLI 是下游脚本的契约，所以校验分支单独成模块、可被 ``--selftest`` 直接断言
（``run.py`` 只负责编排与产物）。所有非法组合都在**起任何子进程之前**变成可行动的
错误（退出码 2），而不是跑到一半才炸。

形态专属旋钮（``--tool`` / ``--cycle-s`` / ``--decode-duty`` / ``--history-tokens``）
的适用范围由形态自己声明（``shapes.KNOBS``）：给了某个形态不消费的旋钮就报错，不静默
忽略——静默忽略会让人以为「那个变量已经测过了」。
"""

from __future__ import annotations

import argparse
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path

import shapes
import orchestrator
from orchestrator import RunConfig

DEFAULT_SCENARIO = "pure_stream"
DEFAULT_LEVELS = "1,10,50,100"
DEFAULT_RATE = 50.0
DEFAULT_DWELL = 20.0
DEFAULT_WARMUP = 5.0
DEFAULT_SHARDS = 4
DEFAULT_SUBSCRIBERS = "per-session"
DEFAULT_HISTORY_TOKENS = 0
DEFAULT_BACKEND = "file"
DEFAULT_OUT_ROOT = Path("target/capacity")
"""默认产物根（相对仓库根；``build_config`` 会解析成绝对路径）。"""

BACKENDS: tuple[str, ...] = ("file", "memory")


class UsageError(ValueError):
    """命令行参数不可用（可行动的错误文案；退出码 2）。"""


def parse_levels(raw: str) -> tuple[int, ...]:
    """``"1,10,100"`` → ``(1, 10, 100)``（正整数、严格升序）。"""
    levels: list[int] = []
    for chunk in raw.split(","):
        text = chunk.strip()
        if not text:
            raise UsageError(f"--levels has an empty entry: {raw!r}")
        try:
            value = int(text)
        except ValueError as exc:
            raise UsageError(f"--levels entry {text!r} is not an integer") from exc
        if value < 1:
            raise UsageError(f"--levels entry {value} must be >= 1")
        levels.append(value)
    if not levels:
        raise UsageError("--levels is empty")
    if sorted(set(levels)) != levels:
        raise UsageError(
            f"--levels must be strictly ascending with no duplicates, got {raw!r}"
        )
    return tuple(levels)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="capacity",
        description=(
            "capacity sweep: N concurrent gateway sessions driven by a fake provider, "
            "sampled for CPU / RSS / fds / delivery latency"
        ),
    )
    parser.add_argument(
        "--scenario",
        default=DEFAULT_SCENARIO,
        help=f"load shape (available: {', '.join(shapes.names())})",
    )
    parser.add_argument(
        "--levels",
        default=DEFAULT_LEVELS,
        help=f"ascending comma-separated session counts (default: {DEFAULT_LEVELS})",
    )
    parser.add_argument(
        "--rate",
        type=float,
        default=DEFAULT_RATE,
        help=(
            "events/s per session — one event is one upstream SSE delta frame "
            f"(default: {DEFAULT_RATE:g})"
        ),
    )
    parser.add_argument(
        "--chunk",
        type=int,
        default=shapes.DEFAULT_CHUNK,
        help=(
            "characters carried by one event (default: "
            f"{shapes.DEFAULT_CHUNK}); 1 is the worst case, real providers usually "
            "frame 1~20 characters per delta"
        ),
    )
    parser.add_argument(
        "--dwell",
        type=float,
        default=DEFAULT_DWELL,
        help=f"sample window per level (s, default: {DEFAULT_DWELL:g})",
    )
    parser.add_argument(
        "--warmup",
        type=float,
        default=DEFAULT_WARMUP,
        help=f"warmup before the window (s, dropped; default: {DEFAULT_WARMUP:g})",
    )
    parser.add_argument(
        "--shards",
        type=int,
        default=DEFAULT_SHARDS,
        help=f"driver shard subprocesses (default: {DEFAULT_SHARDS})",
    )
    parser.add_argument(
        "--subscribers",
        choices=("per-session", "per-shard"),
        default=DEFAULT_SUBSCRIBERS,
        help=f"WS subscriber topology (default: {DEFAULT_SUBSCRIBERS})",
    )
    parser.add_argument(
        "--backend",
        choices=BACKENDS,
        default=DEFAULT_BACKEND,
        help=(
            "session storage backend passed to /api/session/create (default: "
            f"{DEFAULT_BACKEND}); memory = in-process, no history.jsonl, no fsync — "
            "the file/memory A/B attributes the per-turn fixed cost"
        ),
    )
    parser.add_argument(
        "--history-tokens",
        dest="history_tokens",
        type=int,
        default=DEFAULT_HISTORY_TOKENS,
        help=(
            "pre-seeded history per session (tokens, estimated); implemented for: "
            + ", ".join(name for name in shapes.names() if _supports_history(name))
        ),
    )
    parser.add_argument(
        "--tool",
        default=shapes.DEFAULT_TOOL,
        help=(
            f"tool used by tool cycles (default: {shapes.DEFAULT_TOOL}); "
            "read = Read a small workspace file, bash = Bash echo (subprocess)"
        ),
    )
    parser.add_argument(
        "--cycle-s",
        dest="cycle_s",
        type=float,
        default=shapes.DEFAULT_CYCLE_S,
        help=(
            "target cycle duration for tool cycles (s, default: "
            f"{shapes.DEFAULT_CYCLE_S:g}); 0 = no pacing"
        ),
    )
    parser.add_argument(
        "--decode-duty",
        dest="decode_duty",
        type=float,
        default=shapes.DEFAULT_DECODE_DUTY,
        help=(
            "fraction of sessions streaming in the mixed shape (default: "
            f"{shapes.DEFAULT_DECODE_DUTY:g})"
        ),
    )
    parser.add_argument(
        "--out",
        help=f"output dir (default: {DEFAULT_OUT_ROOT}/<utc timestamp>)",
    )
    parser.add_argument(
        "--selftest",
        action="store_true",
        help="offline self-check (small N, short window, no network)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print a single JSON summary line on stdout",
    )
    return parser.parse_args(argv)


def _supports_history(name: str) -> bool:
    try:
        return "history_tokens" in frozenset(getattr(shapes.resolve(name), "KNOBS", ()))
    except shapes.ShapeError:  # pragma: no cover - 注册表与模块的一致性由自测覆盖
        return False


def build_config(args: argparse.Namespace) -> RunConfig:
    """参数 → :class:`RunConfig`（所有非法组合在这里变成可行动的错误）。"""
    shapes.resolve(args.scenario)  # 未知名这里就抛（附可选项）
    levels = parse_levels(args.levels)
    if args.rate <= 0:
        raise UsageError(f"--rate must be > 0, got {args.rate}")
    if args.chunk < 1:
        raise UsageError(f"--chunk must be >= 1, got {args.chunk}")
    if args.dwell <= 0:
        raise UsageError(f"--dwell must be > 0, got {args.dwell}")
    if args.warmup < 0:
        raise UsageError(f"--warmup must be >= 0, got {args.warmup}")
    if args.shards < 1:
        raise UsageError(f"--shards must be >= 1, got {args.shards}")
    if args.history_tokens < 0:
        raise UsageError(f"--history-tokens must be >= 0, got {args.history_tokens}")
    if args.cycle_s < 0:
        raise UsageError(f"--cycle-s must be >= 0, got {args.cycle_s}")
    if not 0.0 <= args.decode_duty <= 1.0:
        raise UsageError(f"--decode-duty must be within [0, 1], got {args.decode_duty}")
    # 必须绝对：分片以 ``cwd=<档位目录>`` 起，相对路径会让它找不到自己的配置文件。
    out = (
        (Path(args.out) if args.out else DEFAULT_OUT_ROOT / stamp())
        .expanduser()
        .resolve()
    )
    cfg = RunConfig(
        scenario=args.scenario,
        levels=levels,
        rate=float(args.rate),
        dwell=float(args.dwell),
        warmup=float(args.warmup),
        shards=int(args.shards),
        subscribers=str(args.subscribers),
        history_tokens=int(args.history_tokens),
        out=out,
        chunk=int(args.chunk),
        backend=str(args.backend),
        tool=str(args.tool),
        cycle_s=float(args.cycle_s),
        decode_duty=float(args.decode_duty),
    )
    # 形态级的校验（旋钮适用范围 / 契约 / 逐会话 model）在起任何子进程之前跑一遍：
    # 用最小档位干跑一次 plan()，非法组合在这里变成退出码 2。
    try:
        plan = shapes.build_plan(orchestrator.level_args(cfg, 1))
    except ValueError as exc:  # 含 ShapeError 与形态自己的参数校验
        raise UsageError(str(exc)) from exc
    if plan.window_mode == "hydrate" and cfg.backend == "memory":
        raise UsageError(
            "--backend memory cannot be combined with a hydration shape: in-process "
            "sessions do not survive a gateway restart, so every subscribe would 404"
        )
    return cfg


def stamp() -> str:
    """UTC 时间戳（``--out`` 的默认目录名）。"""
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
