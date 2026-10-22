#!/usr/bin/env python
"""scripts/dev/bench_cache_ledger.py — 缓存账本读 / 写路径的微基准（跨版本可比）。

两侧（会话目录里的 `cache_ledger.json` 边车 / 进程级 `wing.db`）都能跑这个脚本：
账本构造口按签名自动选择（`CacheLedger(session_id)` 是 wing.db 版，
`CacheLedger(log, session_id)` 是边车文件版），因此同一组参数在两棵 worktree 里跑出来的
数字可以直接并排放。

    uv run python scripts/dev/bench_cache_ledger.py                 # 典型 40 轮会话
    uv run python scripts/dev/bench_cache_ledger.py --calls 200 --per-round 2   # 环形上界形态

造数模型（一次调用一段前缀）：`system:base` + `system:wire` + 每个工具一段 + 每轮 2 条
`history:` 段（提问 / 回答）。第 k 轮的调用因此有 `2 + 工具数 + 2k` 段——和真实会话一样，
**越靠后的调用段落表越长**（这是账本读放大假象的来源，别把它抹平）。

量的两件事：
  - ``report``：`GET /api/session/cache` / `wing cache` 的读路径（读整本账本 + 投影）；
  - ``record``：每次 LLM 调用之后的记账（`ContextManager.observe_cache_call`）。
"""

from __future__ import annotations

import argparse
import os
import statistics
import tempfile
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

os.environ.setdefault(
    "WING_HOME", tempfile.mkdtemp(prefix="wing-bench-home-")
)  # 必须在导入 wing 之前：进程级库按它定位
os.environ.setdefault(
    "WING_SESSIONS_PATH", tempfile.mkdtemp(prefix="wing-bench-sessions-")
)

from wing.context import CacheLedger  # noqa: E402
from wing.schema import CachePart  # noqa: E402

TOOLS = (
    "Bash",
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "AskUserQuestion",
    "TodoWrite",
)


def segments_for(round_index: int, per_round: int) -> list[CachePart]:
    """第 round_index 轮调用的前缀（段数随轮次线性增长）。"""
    parts = [
        CachePart.text("system:base", "SYS-PROMPT " * 40),
        CachePart.text("system:wire", '{"system":"SYS-PROMPT"}'),
    ]
    parts += [
        CachePart.text(f"tools:{name}", f"{name} tool declaration") for name in TOOLS
    ]
    for past in range(round_index + 1):
        for slot in range(per_round):
            parts.append(
                CachePart.text(
                    f"history:r{past:03d}-{slot}:user",
                    f"message body {past}-{slot}",
                )
            )
    return parts


def make_ledger(session_id: str, directory: Path) -> Any:
    """两个版本的构造口：wing.db 版只要 session id，边车版要一条 MessageLog。"""
    try:
        return CacheLedger(session_id)
    except TypeError:
        from wing.store.file import FileMessageLog

        return CacheLedger(FileMessageLog(directory / session_id), session_id)


def measure(
    call: Callable[[], Any], *, rounds: int, warmup: int = 3
) -> dict[str, float]:
    for _ in range(warmup):
        call()
    samples: list[float] = []
    for _ in range(rounds):
        started = time.perf_counter()
        call()
        samples.append((time.perf_counter() - started) * 1000)
    samples.sort()
    return {
        "p50": round(statistics.median(samples), 3),
        "p95": round(samples[min(len(samples) - 1, int(len(samples) * 0.95))], 3),
        "mean": round(statistics.fmean(samples), 3),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--calls", type=int, default=40, help="账本里保留多少次调用")
    parser.add_argument(
        "--per-round", type=int, default=2, help="每轮长几条 history 段"
    )
    parser.add_argument("--rounds", type=int, default=50, help="每个测量点跑多少轮")
    args = parser.parse_args()

    directory = Path(tempfile.mkdtemp(prefix="wing-bench-ledger-"))
    session_id = "bench"
    ledger = make_ledger(session_id, directory)

    record_samples: list[float] = []
    for index in range(args.calls):
        parts = segments_for(index, args.per_round)
        started = time.perf_counter()
        ledger.record(
            parts,
            purpose="main",
            model="bench-model",
            provider="bench",
            prompt=1000 + index * 500,
            cached=max(0, 1000 + (index - 1) * 500),
        )
        record_samples.append((time.perf_counter() - started) * 1000)

    report_20 = measure(lambda: ledger.report(limit=20), rounds=args.rounds)
    report_all = measure(lambda: ledger.report(limit=200), rounds=args.rounds)
    rewrite = measure(
        lambda: ledger.record(
            segments_for(args.calls, args.per_round),
            purpose="main",
            model="bench-model",
            provider="bench",
            prompt=10_000,
            cached=9_000,
        ),
        rounds=args.rounds,
    )

    session_bytes = sum(
        path.stat().st_size for path in directory.rglob("*") if path.is_file()
    )
    # 库的落盘量：先 checkpoint 回主库（WAL 会让主库文件看起来是空的），再算总和。
    core = Path(os.environ["WING_HOME"]) / "core"
    if (core / "wing.db").exists():
        from wing.db import get_database

        get_database().execute("PRAGMA wal_checkpoint(TRUNCATE)")
    database_bytes = sum(
        path.stat().st_size for path in core.glob("wing.db*") if path.is_file()
    )
    print(f"calls={args.calls} per_round={args.per_round} rounds={args.rounds}")
    print(f"record (bulk, 首次建账): p50={statistics.median(record_samples):.3f} ms")
    print(f"record (稳态, 再记一条): {rewrite}")
    print(f"report limit=20:  {report_20}")
    print(f"report limit=200: {report_all}")
    print(f"会话目录落盘字节: {session_bytes}")
    print(f"wing.db(+wal) 字节: {database_bytes}")


if __name__ == "__main__":
    main()
