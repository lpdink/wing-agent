#!/usr/bin/env python3
"""命令行面：参数解析 + 校验（``--scenario`` / ``--levels`` 等冻结名字的唯一来源）。

冻结的 CLI 是下游脚本的契约，所以校验分支单独成模块、可被 ``--selftest`` 直接断言
（``run.py`` 只负责编排与产物）。所有非法组合都在**起任何子进程之前**变成可行动的
错误（退出码 2），而不是跑到一半才炸。

旗标名只声明一次（:data:`ARG_SPECS`）：argparse 由它生成，``run.json`` 的 ``meta.args``
与报告里的复现命令都由它取值——否则一次加旗标要在四个地方同步，漏掉一处就会产出一条
「看起来正常但跑不回来」的复现命令。

形态专属旋钮（``--tool`` / ``--cycle-s`` / ``--decode-duty`` / ``--history-tokens``）
的适用范围由形态自己声明（各形态模块的 ``KNOBS``）：给了某个形态不消费的旋钮就报错，不静默
忽略——静默忽略会让人以为「那个变量已经测过了」。
"""

from __future__ import annotations

import argparse
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any

import shapes

if TYPE_CHECKING:  # 只用于类型：``orchestrator`` 会拖进 httpx 与 probe 的运行时依赖
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
SUBSCRIBER_MODES: tuple[str, ...] = ("per-session", "per-shard")


class UsageError(ValueError):
    """命令行参数不可用（可行动的错误文案；退出码 2）。"""


@dataclass(frozen=True)
class ArgSpec:
    """一个旗标：名字 / 取值类型 / 默认值 / 是否进复现命令。"""

    flag: str
    kind: str  # str | int | float | flag | choice
    default: Any
    help: str
    choices: tuple[str, ...] = ()
    mode: bool = False
    """``True`` = 运行模式（``--selftest`` / ``--json`` / ``--criteria``），不进复现命令。"""
    echo: bool = True
    """``True`` = 复现命令里原样回放（默认回放，``--out`` 这种现场参数除外）。"""

    @property
    def dest(self) -> str:
        return self.flag.lstrip("-").replace("-", "_")


ARG_SPECS: tuple[ArgSpec, ...] = (
    ArgSpec(
        "--scenario",
        "str",
        DEFAULT_SCENARIO,
        f"load shape (available: {', '.join(shapes.names())})",
        echo=False,
    ),
    ArgSpec(
        "--levels",
        "str",
        DEFAULT_LEVELS,
        f"ascending comma-separated session counts (default: {DEFAULT_LEVELS})",
    ),
    ArgSpec(
        "--rate",
        "float",
        DEFAULT_RATE,
        "events/s per session — one event is one upstream SSE delta frame "
        f"(default: {DEFAULT_RATE:g}); streaming shapes only",
    ),
    ArgSpec(
        "--chunk",
        "int",
        shapes.DEFAULT_CHUNK,
        "characters carried by one event "
        f"(default: {shapes.DEFAULT_CHUNK}); 1 is the worst case, real providers "
        "usually frame 1~20 characters per delta",
    ),
    ArgSpec(
        "--dwell",
        "float",
        DEFAULT_DWELL,
        f"sample window per level (s, default: {DEFAULT_DWELL:g})",
    ),
    ArgSpec(
        "--warmup",
        "float",
        DEFAULT_WARMUP,
        f"warmup before the window (s, dropped; default: {DEFAULT_WARMUP:g})",
    ),
    ArgSpec(
        "--shards",
        "int",
        DEFAULT_SHARDS,
        f"driver shard subprocesses (default: {DEFAULT_SHARDS})",
    ),
    ArgSpec(
        "--subscribers",
        "choice",
        DEFAULT_SUBSCRIBERS,
        f"WS subscriber topology (default: {DEFAULT_SUBSCRIBERS})",
        choices=SUBSCRIBER_MODES,
    ),
    ArgSpec(
        "--backend",
        "choice",
        DEFAULT_BACKEND,
        "session storage backend passed to /api/session/create (default: "
        f"{DEFAULT_BACKEND}); memory = in-process, no history.jsonl, no fsync — the "
        "file/memory A/B attributes the per-turn fixed cost",
        choices=BACKENDS,
    ),
    ArgSpec(
        "--history-tokens",
        "int",
        DEFAULT_HISTORY_TOKENS,
        "pre-seeded history per session (tokens, estimated)",
    ),
    ArgSpec(
        "--tool",
        "str",
        shapes.DEFAULT_TOOL,
        f"tool used by tool cycles (default: {shapes.DEFAULT_TOOL}); "
        "read = Read a small workspace file, bash = Bash echo (subprocess)",
    ),
    ArgSpec(
        "--cycle-s",
        "float",
        shapes.DEFAULT_CYCLE_S,
        "target cycle duration for tool cycles (s, default: "
        f"{shapes.DEFAULT_CYCLE_S:g}); 0 = no pacing",
    ),
    ArgSpec(
        "--decode-duty",
        "float",
        shapes.DEFAULT_DECODE_DUTY,
        "fraction of sessions streaming in the mixed shape (default: "
        f"{shapes.DEFAULT_DECODE_DUTY:g})",
    ),
    ArgSpec(
        "--out",
        "str",
        None,
        f"output dir (default: {DEFAULT_OUT_ROOT}/<utc timestamp>)",
        echo=False,
    ),
    ArgSpec(
        "--selftest",
        "flag",
        False,
        "offline self-check (no network, short window)",
        mode=True,
    ),
    ArgSpec(
        "--criteria",
        "flag",
        False,
        "print the degradation criteria and exit",
        mode=True,
    ),
    ArgSpec(
        "--json",
        "flag",
        False,
        "print a single JSON summary line on stdout",
        mode=True,
    ),
)
"""旗标名、默认值、以及「哪些进复现命令」的**唯一**声明处。"""


def _add_argument(parser: argparse.ArgumentParser, spec: ArgSpec) -> None:
    kwargs: dict[str, Any] = {"help": spec.help}
    if spec.kind == "flag":
        parser.add_argument(spec.flag, action="store_true", **kwargs)
        return
    kwargs["default"] = spec.default
    if spec.kind == "choice":
        kwargs["choices"] = spec.choices
    elif spec.kind in ("int", "float"):
        kwargs["type"] = int if spec.kind == "int" else float
    parser.add_argument(spec.flag, **kwargs)


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


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="capacity",
        description=(
            "capacity sweep: N concurrent gateway sessions driven by a fake provider, "
            "sampled for CPU / RSS / fds / delivery latency"
        ),
    )
    for spec in ARG_SPECS:
        _add_argument(parser, spec)
    return parser


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    return build_parser().parse_args(argv)


def measurement_args(cfg: RunConfig) -> dict[str, Any]:
    """``meta.args``：把回放用的旗标取值投影成 JSON 友好的字典。"""
    args: dict[str, Any] = {}
    for spec in ARG_SPECS:
        if spec.mode or not spec.echo:
            continue
        value = getattr(cfg, spec.dest)
        args[spec.dest] = list(value) if isinstance(value, tuple) else value
    return args


def build_config(args: argparse.Namespace) -> RunConfig:
    """参数 → :class:`RunConfig`（所有非法组合在这里变成可行动的错误）。"""
    from orchestrator import RunConfig, level_args

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
        plan = shapes.build_plan(level_args(cfg, 1))
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
