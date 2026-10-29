#!/usr/bin/env python3
"""容量测量 CLI：阶梯扫描 N 个并发会话，产出 ``run.json`` + ``report.md`` + 每档原始样本。

    uv run python scripts/capacity/run.py --scenario pure_stream \
        --levels 1,10,50,100 --rate 50 --dwell 20

回答的是「网关能扛住多少个并发**活跃**会话、从哪里开始退化」——不是回归测量
（那是 ``scripts/perf/``），也不是行为断言（那是 ``libs/wing-probe/``）。
进程拓扑、口径与产物字段见同目录 ``README.md``（「进程拓扑」一节解释为什么这样分层）。
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import cli
import orchestrator
import proc
import report
import shapes
import verdict
from cli import UsageError
from orchestrator import DELIVERY_UNAVAILABLE, RunConfig

TOOL = "capacity"
SCHEMA = 1
EXIT_OK = 0
EXIT_USAGE = 2
EXIT_LEVEL_FAILED = 3


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    return cli.parse_args(argv)


def build_config(args: argparse.Namespace) -> RunConfig:
    return cli.build_config(args)


def write_json(path: Path, payload: Any) -> None:
    """原子写 JSON（tmp + rename）；NaN / Infinity 直接失败（非标准 JSON）。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(payload, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def _fill_slope(levels: list[dict[str, Any]]) -> None:
    """``gateway.rss_mb_slope_per_session`` = 与上一档末次 RSS 的相邻差斜率。

    每档都是新网关进程、窗口末是稳态，所以相邻差即「每多一个会话多占多少内存」。
    最小档没有上一档 → 保持 ``None``。
    """
    for index in range(1, len(levels)):
        previous, current = levels[index - 1], levels[index]
        rss_now = (current.get("metrics") or {}).get("gateway.rss_mb_end")
        rss_previous = (previous.get("metrics") or {}).get("gateway.rss_mb_end")
        delta_sessions = int(current["sessions"]) - int(previous["sessions"])
        if rss_now is None or rss_previous is None or delta_sessions <= 0:
            continue
        current["metrics"]["gateway.rss_mb_slope_per_session"] = round(
            (rss_now - rss_previous) / delta_sessions, 3
        )


def _fill_verdicts(levels: list[dict[str, Any]], meta: dict[str, Any]) -> None:
    """把判据结果写进每档（``verdict.py`` 是唯一实现，报告会自己再算一遍）。

    跨档比较（baseline = 最小 N 档）只能在阶梯跑完之后做，所以放在这里而不是
    ``orchestrator.run_level``（单档看不到别的档）。失败的档位保持全 ``null``。
    """
    results = verdict.evaluate_run({"meta": meta, "levels": levels})
    for record, result in zip(levels, results):
        record["verdict"] = verdict.block(result)


async def run_ladder(cfg: RunConfig, *, selftest_run: bool = False) -> dict[str, Any]:
    """跑完一条阶梯并落盘 ``run.json`` / ``report.md``，返回 ``run.json`` 的内容。"""
    started_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    started = time.monotonic()
    cfg.out.mkdir(parents=True, exist_ok=True)
    print(
        f"[capacity] {cfg.scenario}: levels={list(cfg.levels)} rate={cfg.rate} "
        f"dwell={cfg.dwell} warmup={cfg.warmup} shards={cfg.shards} "
        f"subscribers={cfg.subscribers} → {cfg.out}",
        file=sys.stderr,
        flush=True,
    )
    overshoot_s = await orchestrator.measure_pacing(cfg.rate)
    frame_delay_s = max(0.0, 1.0 / cfg.rate - overshoot_s)
    print(
        f"[capacity] pacing: asyncio.sleep overshoot {overshoot_s * 1000:.2f} ms → "
        f"frame delay {frame_delay_s * 1000:.2f} ms (nominal {1000 / cfg.rate:.2f} ms)",
        file=sys.stderr,
        flush=True,
    )
    levels: list[dict[str, Any]] = []
    for sessions in cfg.levels:
        print(f"[capacity] level {sessions}: running", file=sys.stderr, flush=True)
        record = await orchestrator.run_level(
            cfg,
            sessions,
            cfg.out / "raw" / str(sessions),
            frame_delay_s=frame_delay_s,
            tracebacks=not selftest_run,
        )
        levels.append(record)
        print(
            f"[capacity] level {sessions}: {report.summary_line(record)}",
            file=sys.stderr,
            flush=True,
        )
    _fill_slope(levels)

    failures = [
        f"level {record['sessions']}: {record['error']}"
        for record in levels
        if record.get("error")
    ]
    meta = {
        "tool": TOOL,
        "schema": SCHEMA,
        "scenario": cfg.scenario,
        "started_at": started_at,
        "duration_s": round(time.monotonic() - started, 1),
        "args": {
            **cli.measurement_args(cfg),
            "frame_delay_ms": round(frame_delay_s * 1000, 3),
            "sleep_overshoot_ms": round(overshoot_s * 1000, 3),
        },
        "host": proc.host_info(),
        "gateway": _meta_gateway(levels),
        "session_create": _meta_session_create(levels),
        "notes": _meta_notes(levels),
        "failures": failures,
        "out": str(cfg.out),
    }
    if selftest_run:
        meta["selftest"] = True
    _fill_verdicts(levels, meta)
    payload = {"meta": meta, "levels": levels}
    write_json(cfg.out / "run.json", payload)
    (cfg.out / "report.md").write_text(report.render(payload), encoding="utf-8")
    return payload


def _meta_gateway(levels: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """网关身份：取**最后一档**进程（每档都是新进程，逐档身份在 ``levels[i].gateway``）。"""
    for record in reversed(levels):
        gateway = record.get("gateway") or {}
        if gateway.get("pid") is not None:
            return {
                "pid": gateway.get("pid"),
                "version": gateway.get("version"),
                "commit": gateway.get("commit"),
            }
    return {"pid": None, "version": None, "commit": None}


def _meta_session_create(levels: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """建会话成本：逐档数字在 ``levels[i].session_create``，这里是全档汇总。

    有档位根本没拿到数字（环境没起来）时整组写 ``null`` 而不是「按 0 求和」——
    「11 个成功」被读成「11 个会话都建好了」比缺数据更误导。
    """
    stats = [record.get("session_create") or {} for record in levels]
    missing = [
        record["sessions"]
        for record, entry in zip(levels, stats)
        if entry.get("ok") is None
    ]
    if missing:
        return {
            "ok": None,
            "failed": None,
            "total_ms": None,
            "scope": f"not summed: levels {missing} created no measurable sessions",
        }
    return {
        "ok": sum(int(entry["ok"]) for entry in stats),
        "failed": sum(int(entry["failed"] or 0) for entry in stats),
        "total_ms": round(sum(float(entry["total_ms"] or 0.0) for entry in stats), 1),
        "scope": "sum over levels",
    }


def _meta_notes(levels: Sequence[dict[str, Any]]) -> list[str]:
    """全局说明：投递延迟是否可用、是否有档位失败。"""
    notes: list[str] = []
    if any(
        (record.get("samples") or {}).get("delivery_available") is False
        for record in levels
    ):
        notes.append(DELIVERY_UNAVAILABLE)
    failed = [record["sessions"] for record in levels if record.get("error")]
    if failed:
        notes.append(
            f"levels {failed} did not complete; metrics that were not measured are "
            "null (see levels[i].error / levels[i].error_detail)"
        )
    return notes


def _summary(payload: dict[str, Any]) -> dict[str, Any]:
    """``--json`` 的一行摘要（字段见 README「常用命令」的表）。"""
    out = Path(str(payload["meta"].get("out", "")))
    return {
        "ok": not payload["meta"]["failures"],
        "out": str(out),
        "run_json": str(out / "run.json"),
        "report": str(out / "report.md"),
        "levels": [
            {
                "sessions": record["sessions"],
                "gateway_cpu_cores": (record.get("metrics") or {}).get(
                    "gateway.cpu_cores"
                ),
                "delivery_p99_ms": (record.get("metrics") or {}).get("delivery.p99_ms"),
                "tokens_per_second_actual": (record.get("metrics") or {}).get(
                    "tokens_per_second_actual"
                ),
                "events_per_second_actual": (record.get("metrics") or {}).get(
                    "events_per_second_actual"
                ),
                "chars_per_second": (record.get("metrics") or {}).get(
                    "chars_per_second"
                ),
                "turns_per_second": (record.get("metrics") or {}).get(
                    "turns_per_second"
                ),
                "errors": (record.get("metrics") or {}).get("errors"),
                "verdict": (record.get("verdict") or {}).get("verdict"),
                "low_confidence": (record.get("verdict") or {}).get("low_confidence"),
                "error": record.get("error"),
            }
            for record in payload["levels"]
        ],
        "failures": payload["meta"]["failures"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        cfg = build_config(args)  # --selftest 也先过一遍校验：非法参数一律退 2
    except (UsageError, shapes.ShapeError) as exc:
        print(f"[capacity] error: {exc}", file=sys.stderr)
        return EXIT_USAGE
    if args.criteria:
        for key, text in verdict.CRITERIA.items():
            print(f"{key}: {text}")
        return EXIT_OK
    if args.selftest:
        import selftest  # 只在这条路径上依赖自测代码

        return selftest.main(run_ladder=run_ladder)
    try:
        proc.raise_nofile(proc.nofile_target(max(cfg.levels)))
    except proc.ProcError as exc:
        print(f"[capacity] error: {exc}", file=sys.stderr)
        return EXIT_USAGE
    try:
        payload = asyncio.run(run_ladder(cfg))
    except KeyboardInterrupt:  # pragma: no cover - 人工中断
        print("[capacity] interrupted", file=sys.stderr)
        return 130
    summary = _summary(payload)
    if args.json:
        print(json.dumps(summary, ensure_ascii=False))
    else:
        print(f"[capacity] run.json → {cfg.out / 'run.json'}")
        print(f"[capacity] report   → {cfg.out / 'report.md'}")
        for line in payload["meta"]["failures"]:
            print(f"[capacity] failure: {line}", file=sys.stderr)
    return EXIT_OK if summary["ok"] else EXIT_LEVEL_FAILED


if __name__ == "__main__":
    raise SystemExit(main())
