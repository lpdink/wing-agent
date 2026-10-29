#!/usr/bin/env python3
"""离线自测（``run.py --selftest``）：≤60 秒、不联网、不需要 API key、可重复。

五组检查，对应这个工具的四件必须可信的事 + 产物形状：

- ``stats``    百分位秩定义（known answer，含 n=1 / 偶数 / 极值 / 空序列报错）；
- ``pairing``  投递延迟配对（合成 Provider：值可控，覆盖长度不等 / 命中多条 /
               负延迟 / 缺接口抛错 / 无 ``emits_for`` 时的退路）；
- ``proc``     进程采样（自身 CPU 单调、RSS、fd、fd 软限抬升）；
- ``e2e``      走**真实**执行路径的迷你一档（真网关子进程 + 真分片子进程），断言
               ``run.json`` / ``report.md`` / ``raw/<N>/`` 的形状与「缺数据写 null」；
- ``report``   合成 ``run.json`` → markdown 的渲染。

``e2e`` 通过注入的 ``run_ladder`` 复用正式路径——自测里跑的是产品代码，不是另一份
平行实现（否则自测绿了也不能说明工具能用）。
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import metrics
import orchestrator
import proc
import report
import shapes
from orchestrator import RunConfig

#: 自测里 e2e 一档的预算（秒）。总预算 ≤60 s（任务书硬约束），留足余量。
E2E_BUDGET_S = 45.0

LadderRunner = Callable[..., Awaitable[dict[str, Any]]]


class SelftestFailure(AssertionError):
    """一条自测断言失败（附可复现的最小上下文）。"""


def _check(condition: bool, message: str) -> None:
    if not condition:
        raise SelftestFailure(message)


# ── stats ─────────────────────────────────────────────────────


def check_stats() -> None:
    """nearest-rank 的 known answer（秩定义是数字口径的唯一来源）。"""
    _check(metrics.percentile([5.0], 99.0) == 5.0, "single sample must be its own p99")
    _check(
        metrics.percentile([1.0, 2.0, 3.0, 4.0], 50.0) == 2.0,
        "p50 of 4 samples = ceil(0.5*4)=2nd value",
    )
    hundred = [float(value) for value in range(1, 101)]
    _check(metrics.percentile(hundred, 50.0) == 50.0, "p50 of 1..100 must be 50")
    _check(metrics.percentile(hundred, 99.0) == 99.0, "p99 of 1..100 must be 99")
    _check(metrics.percentile(hundred, 99.9) == 100.0, "p99.9 of 1..100 must be 100")
    _check(metrics.percentile(hundred, 100.0) == 100.0, "p100 must be the maximum")
    _check(
        metrics.spread([]) == {},
        "empty samples must yield an empty spread (callers write null, never 0)",
    )
    _check(
        set(metrics.spread(hundred)) == {"p50", "p99", "p999"},
        "spread must expose p50 / p99 / p999",
    )
    try:
        metrics.percentile([], 50.0)
    except ValueError:
        pass
    else:  # pragma: no cover - 口径回归
        raise SelftestFailure("percentile() of an empty sequence must raise")


# ── pairing ───────────────────────────────────────────────────


def _provider(emits: Mapping[int, Sequence[float]], *, with_getter: bool = True) -> Any:
    """合成 Provider：满足上游冻结接口的最小形态（值可控）。"""
    frame_emits = [
        SimpleNamespace(request_index=index, frame_index=frame, at=at)
        for index, values in emits.items()
        for frame, at in enumerate(values)
    ]
    fields: dict[str, Any] = {"frame_emits": frame_emits}
    if with_getter:
        fields["emits_for"] = lambda request_index: list(emits[request_index])
    return SimpleNamespace(**fields)


def _request(index: int, prompt: str) -> Any:
    return SimpleNamespace(
        index=index, body={"messages": [{"role": "user", "content": prompt}]}
    )


def check_last_user_text() -> None:
    """两种 body 形态都要能取到正文（str 与 block 列表）。"""
    _check(
        metrics.last_user_text(_request(0, "hello").body) == "hello",
        "plain string content must round-trip",
    )
    blocks = {
        "messages": [
            {"role": "system", "content": "sys"},
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "load:abc:0",
                        "cache_control": {"type": "ephemeral"},
                    }
                ],
            },
            {"role": "tool", "content": "tool output"},
        ]
    }
    _check(
        metrics.last_user_text(blocks) == "load:abc:0",
        "block-list content must join its text blocks and skip non-user messages",
    )
    _check(
        metrics.last_user_text({"messages": [{"role": "system", "content": "x"}]})
        is None,
        "no user message must yield None",
    )


def check_pairing() -> None:
    """按下标配对 + 四类丢弃原因 + 缺接口抛错。"""
    emits = {0: [100.0, 100.02, 100.04], 1: [200.0, 200.02]}
    provider = _provider(emits)
    requests = [_request(0, "load:s0:0"), _request(1, "load:s1:0")]
    turns = [
        metrics.TurnSample(
            prompt="load:s0:0",
            sent_at=99.9,
            result_at=100.1,
            arrivals=(100.001, 100.023, 100.044),
        ),
        metrics.TurnSample(
            prompt="load:s1:0", sent_at=199.9, result_at=200.1, arrivals=(200.002,)
        ),
    ]
    pairing = metrics.pair_delivery(provider, requests, turns)
    _check(pairing.paired == 3, f"expected 3 paired frames, got {pairing.paired}")
    _check(pairing.dropped == 1, f"expected 1 dropped frame, got {pairing.dropped}")
    _check(
        pairing.reasons.get("length_mismatch") == 1,
        f"length mismatch must be classified, got {dict(pairing.reasons)}",
    )
    _check(
        sorted(round(value, 3) for value in pairing.latencies_ms) == [1.0, 3.0, 4.0],
        f"latency must be arrival - emit, got {pairing.latencies_ms}",
    )

    # 未知 prompt / 同一 prompt 命中多条（重试）→ 整轮丢弃，不猜。
    unknown = metrics.pair_delivery(
        provider,
        requests,
        [metrics.TurnSample("load:zzz:0", 1.0, 2.0, (100.0, 100.1))],
    )
    _check(unknown.paired == 0 and unknown.dropped == 2, "unknown prompt must drop")
    _check(
        unknown.reasons.get("unknown_prompt") == 1, "unknown prompt must be classified"
    )
    ambiguous = metrics.pair_delivery(
        provider,
        [_request(0, "load:s0:0"), _request(1, "load:s0:0")],
        [metrics.TurnSample("load:s0:0", 0.0, 1.0, (100.0, 100.02, 100.04))],
    )
    _check(
        ambiguous.paired == 0 and ambiguous.dropped == 3, "ambiguous prompt must drop"
    )
    _check(
        ambiguous.reasons.get("ambiguous_prompt") == 1,
        "ambiguous prompt must be classified",
    )

    # 负延迟 = 错位，不写进样本。
    negative = metrics.pair_delivery(
        _provider({0: [100.0, 100.02]}),
        [_request(0, "p")],
        [metrics.TurnSample("p", 0.0, 1.0, (99.9, 100.03))],
    )
    _check(
        negative.paired == 1 and negative.dropped == 1,
        "negative latency must be dropped",
    )
    _check(
        negative.reasons.get("negative_latency") == 1,
        "negative latency must be classified",
    )

    # 没有 emits_for 时退到 frame_emits（按 frame_index 排序）。
    fallback = _provider({0: [100.0, 100.02]}, with_getter=False)
    _check(
        metrics.frame_emits_for(fallback, 0) == [100.0, 100.02],
        "frame_emits fallback must keep frame order",
    )

    # 缺接口必须显式失败（而不是返回空列表 → 静默「无延迟数据」）。
    missing = _provider({}, with_getter=False)
    del missing.frame_emits
    _check(not metrics.has_frame_emits(missing), "capability probe must report absence")
    for call in (
        lambda: metrics.frame_emits_for(missing, 0),
        lambda: metrics.pair_delivery(
            missing,
            [_request(0, "p")],
            [metrics.TurnSample("p", 0.0, 1.0, (1.0,))],
        ),
    ):
        try:
            call()
        except metrics.DeliveryUnavailable as exc:
            _check(
                "frame_emits not available" in str(exc),
                f"error text must be greppable: {exc}",
            )
        else:  # pragma: no cover - 红线回归
            raise SelftestFailure("missing frame_emits must raise DeliveryUnavailable")


# ── proc ──────────────────────────────────────────────────────


def check_proc() -> None:
    """进程采样与 fd 软限：自身进程是最容易验证的样本。"""
    first = proc.self_cpu_seconds()
    time.sleep(0.02)
    second = proc.self_cpu_seconds()
    _check(second >= first, "self CPU must be monotonic")
    pid = os.getpid()
    _check(
        proc.process_cpu_seconds(pid) > 0.0, "process CPU must be > 0 for a live pid"
    )
    _check(proc.process_rss_mb(pid) > 0.0, "RSS must be > 0")
    _check(
        proc.process_fds(pid) >= 3,
        "a live interpreter has at least stdin/stdout/stderr",
    )
    target = proc.nofile_target(10)
    soft, _hard = proc.raise_nofile(target)
    _check(soft >= target, f"fd soft limit must reach {target}, got {soft}")
    host = proc.host_info()
    _check(int(host["cpus"] or 0) >= 1, "host_info must report at least one cpu")
    _check(float(host["mem_gb"] or 0) > 0.0, "host_info must report memory")


# ── report ────────────────────────────────────────────────────


def _synthetic_run() -> dict[str, Any]:
    return {
        "meta": {
            "tool": "capacity",
            "schema": 1,
            "scenario": "pure_stream",
            "started_at": "2026-01-01T00:00:00Z",
            "duration_s": 12.0,
            "args": {
                "levels": [1, 10],
                "rate": 50.0,
                "dwell": 5.0,
                "warmup": 2.0,
                "shards": 1,
                "subscribers": "per-shard",
                "history_tokens": 0,
            },
            "host": {
                "cpus": 12,
                "mem_gb": 16.0,
                "platform": "darwin",
                "ulimit_n": 4096,
            },
            "gateway": {"pid": 1, "version": "test", "commit": "abcdef0"},
            "session_create": {"ok": 11, "failed": 0, "total_ms": 12.0},
            "notes": [],
            "failures": [],
        },
        "levels": [
            {
                "sessions": 1,
                "wall_s": 5.0,
                "metrics": {
                    **dict.fromkeys(orchestrator.METRIC_KEYS),
                    "gateway.cpu_cores": 0.1,
                    "gateway.rss_mb_peak": 100.0,
                    "gateway.rss_mb_end": 101.0,
                    "gateway.fds_peak": 30,
                    "delivery.p50_ms": 1.0,
                    "delivery.p99_ms": 2.0,
                    "delivery.p999_ms": 3.0,
                    "turn.p50_ms": 5000.0,
                    "turn.p99_ms": 5100.0,
                    "tokens_per_second_actual": 50.0,
                    "errors": 0,
                    "slow_client_recycles": 0,
                },
                "rig": {
                    "provider_cpu_cores": 0.2,
                    "shard_cpu_cores": [0.1],
                    "host_loadavg": [1.0, 1.0, 1.0],
                },
                "samples": {
                    "count": 250,
                    "dropped": 0,
                    "text_frames": 250,
                    "turns": 1,
                    "delivery_available": True,
                },
                "session_create": {"ok": 1, "failed": 0, "total_ms": 4.0},
                "gateway": {"pid": 1, "url": "http://127.0.0.1:1"},
            },
            {
                "sessions": 10,
                "wall_s": 5.0,
                "metrics": {
                    **dict.fromkeys(orchestrator.METRIC_KEYS),
                    "gateway.cpu_cores": 1.0,
                    "gateway.rss_mb_peak": 200.0,
                    "gateway.rss_mb_end": 202.0,
                    "gateway.fds_peak": 300,
                    "tokens_per_second_actual": 500.0,
                    "errors": 2,
                    "slow_client_recycles": 1,
                },
                "rig": {
                    "provider_cpu_cores": 0.5,
                    "shard_cpu_cores": [0.4, 0.3],
                    "host_loadavg": None,
                },
                "samples": {
                    "count": None,
                    "dropped": None,
                    "text_frames": 2500,
                    "turns": 10,
                    "delivery_available": False,
                },
                "session_create": {"ok": 10, "failed": 0, "total_ms": 8.0},
                "gateway": {"pid": 2, "url": "http://127.0.0.1:2"},
            },
        ],
    }


def check_report() -> None:
    """合成 run.json → markdown：行数、缺数据占位、不可用横幅。"""
    text = report.render(_synthetic_run())
    _check(
        text.startswith("# capacity — pure_stream"), "report must start with its title"
    )
    _check("| 1 |" in text and "| 10 |" in text, "each level must get a row")
    _check("| 0.10 |" in text, "gateway cpu must be rendered with two decimals")
    _check("投递延迟不可用" in text, "unavailable delivery must be called out up front")
    _check("— / —" in text, "missing turn metrics must render as an em dash, not 0")
    _check("nearest-rank" in text, "report must state the percentile basis")
    _check(
        "0.40, 0.30" in text,
        "per-shard CPU must be listed (rig quality signal)",
    )
    line = report.summary_line(_synthetic_run()["levels"][0])
    _check("N=1:" in line and "gw_cpu=" in line, "summary line must name the level")


# ── e2e ───────────────────────────────────────────────────────


def _e2e_config(out: Path) -> RunConfig:
    return RunConfig(
        scenario="pure_stream",
        levels=(2,),
        rate=20.0,
        dwell=3.0,
        warmup=1.0,
        shards=1,
        subscribers="per-shard",
        history_tokens=0,
        out=out,
    )


def check_e2e(run_ladder: LadderRunner) -> None:
    """迷你一档走真实路径：真网关子进程 + 真分片子进程 + 真产物。"""
    with tempfile.TemporaryDirectory(prefix="capacity-selftest-") as tmp:
        out = Path(tmp) / "run"
        cfg = _e2e_config(out)
        started = time.monotonic()
        payload = asyncio.run(run_ladder(cfg, selftest_run=True))
        elapsed = time.monotonic() - started
        _check(
            elapsed < E2E_BUDGET_S,
            f"e2e level took {elapsed:.1f}s, budget {E2E_BUDGET_S:.0f}s",
        )

        meta = payload["meta"]
        _check(meta["tool"] == "capacity" and meta["schema"] == 1, "meta identity")
        _check(meta.get("selftest") is True, "meta.selftest must mark a self-check run")
        _check(int(meta["host"]["ulimit_n"]) > 0, "meta.host.ulimit_n must be recorded")
        _check(meta["failures"] == [], f"e2e run must not fail: {meta['failures']}")
        levels = payload["levels"]
        _check(len(levels) == 1, "one level requested, one level reported")
        level = levels[0]
        _check("error" not in level, f"level failed: {level.get('error')}")

        _check(level["sessions"] == 2, "level must report the requested N")
        _check(level["session_create"]["ok"] == 2, "both sessions must be created")
        metrics_block = level["metrics"]
        for key in orchestrator.METRIC_KEYS:
            _check(key in metrics_block, f"missing frozen metric key {key!r}")
        _check(
            metrics_block["gateway.cpu_cores"] is not None
            and metrics_block["gateway.cpu_cores"] >= 0.0,
            "gateway cpu must be sampled",
        )
        _check(
            metrics_block["gateway.fds_peak"] is not None
            and metrics_block["gateway.fds_peak"] >= 3,
            "gateway fd peak must be sampled",
        )
        _check(
            metrics_block["tokens_per_second_actual"] is not None
            and metrics_block["tokens_per_second_actual"] > 0.0,
            "token rate must be non-zero for a streaming load",
        )
        _check(
            metrics_block["errors"] == 0,
            f"no errors expected: {metrics_block['errors']}",
        )
        _check(level["samples"]["text_frames"] > 0, "text frames must be counted")
        _check(
            level["samples"]["turns"] >= 1,
            "at least one turn must complete inside the window",
        )

        samples = level["samples"]
        if samples["delivery_available"]:
            p50 = metrics_block["delivery.p50_ms"]
            p99 = metrics_block["delivery.p99_ms"]
            _check(
                p50 is not None and p99 is not None,
                "available delivery must have values",
            )
            _check(0.0 <= p50 <= p99, f"latency order violated: p50={p50} p99={p99}")
            _check(
                samples["count"] is not None and samples["count"] > 0,
                "available delivery must pair at least one frame",
            )
        else:  # 上游接口未就位的形态：必须是「显式不可用」，不能写 0
            _check(
                metrics_block["delivery.p50_ms"] is None,
                "unavailable delivery must be null, not 0",
            )
            _check(
                samples["count"] is None and samples["dropped"] is None,
                "unavailable delivery must not fake sample counts",
            )
            notes = list(meta["notes"]) + list(level.get("notes") or [])
            _check(
                any("frame_emits not available" in note for note in notes),
                "unavailable delivery must be explained in meta.notes / level notes",
            )

        # 产物形状：run.json / report.md / raw/<N>/ 分片样本
        run_json = out / "run.json"
        report_md = out / "report.md"
        shard_file = out / "raw" / "2" / "shard-0.samples.json"
        _check(run_json.is_file(), f"run.json must exist ({run_json})")
        _check(report_md.is_file(), f"report.md must exist ({report_md})")
        _check(shard_file.is_file(), f"raw shard samples must exist ({shard_file})")
        on_disk = json.loads(run_json.read_text(encoding="utf-8"))
        _check(
            on_disk["levels"][0]["sessions"] == 2, "run.json must round-trip the level"
        )
        _check(
            "# capacity — pure_stream" in report_md.read_text(encoding="utf-8"),
            "report.md must be rendered markdown",
        )
        shard = json.loads(shard_file.read_text(encoding="utf-8"))
        _check(shard["shard"] == 0, "shard sample file must name its shard")
        _check(
            isinstance(shard["window"], dict)
            and shard["window"]["end"] > shard["window"]["start"],
            "shard sample file must carry a window",
        )
        _check(len(shard["turns"]) >= 1, "shard must report at least one turn sample")


# ── 入口 ──────────────────────────────────────────────────────


def checks() -> list[tuple[str, Callable[[], None]]]:
    return [
        ("stats.percentile", check_stats),
        ("pairing.last_user_text", check_last_user_text),
        ("pairing.delivery", check_pairing),
        ("proc.sampling", check_proc),
        ("report.render", check_report),
        ("shapes.contract", check_shapes),
    ]


def check_shapes() -> None:
    """形态契约：注册表能解析，剧本覆盖整档所需轮次，prompt 文本全局唯一。"""
    shape = shapes.resolve("pure_stream")
    sessions, rate, span = 3, 20.0, 6.0
    script = shape.script(sessions=sessions, rate=rate, span_s=span)
    per_session = int(span / shape.turn_seconds(span)) + 2  # 上限的下界（floor）
    _check(
        len(script) >= sessions * per_session,
        f"script must hold at least {sessions * per_session} turns, got {len(script)}",
    )
    first = shape.prompt_text("sess-a", 0)
    _check(first != shape.prompt_text("sess-a", 1), "prompt text must vary by turn")
    _check(first != shape.prompt_text("sess-b", 0), "prompt text must vary by session")
    try:
        shapes.resolve("no-such-shape")
    except shapes.ShapeError as exc:
        _check("pure_stream" in str(exc), f"unknown shape must list the choices: {exc}")
    else:  # pragma: no cover - 注册表回归
        raise SelftestFailure("unknown scenario must raise ShapeError")


def main(*, run_ladder: LadderRunner) -> int:
    """跑完全部自测；全绿退出 0，否则打印失败详情并退出 1。"""
    started = time.monotonic()
    passed = 0
    failures: list[str] = []
    for name, check in checks():
        try:
            check()
        except Exception as exc:  # noqa: BLE001 - 自测要把每条失败都列出来
            failures.append(f"{name}: {type(exc).__name__}: {exc}")
            print(f"[selftest] FAIL {name}: {exc}", file=sys.stderr)
            continue
        passed += 1
        print(f"[selftest] ok   {name}")
    try:
        check_e2e(run_ladder)
    except Exception as exc:  # noqa: BLE001 - 同上
        failures.append(f"e2e: {type(exc).__name__}: {exc}")
        print(f"[selftest] FAIL e2e: {exc}", file=sys.stderr)
    else:
        passed += 1
        print("[selftest] ok   e2e")
    elapsed = time.monotonic() - started
    print(f"[selftest] {passed} passed · {len(failures)} failed · {elapsed:.1f}s")
    if failures:
        for failure in failures:
            print(f"[selftest] failure: {failure}", file=sys.stderr)
        return 1
    return 0


__all__ = [
    "E2E_BUDGET_S",
    "SelftestFailure",
    "check_e2e",
    "check_pairing",
    "check_proc",
    "check_report",
    "check_shapes",
    "check_stats",
    "checks",
    "main",
]
