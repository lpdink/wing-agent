#!/usr/bin/env python3
"""离线自测（``run.py --selftest``）：≤60 秒、不联网、不需要 API key、可重复。

十组断言（见 :func:`checks`）+ ``e2e``，对应这个工具必须可信的几件事：

- ``stats.percentile``        百分位秩定义（known answer，含 n=1 / 偶数 / 极值 / 空序列报错）
- ``pairing.last_user_text``  配对键的提取（str / block 列表两种 body 形态）
- ``pairing.delivery``        按下标配对 + 四类丢弃原因 + 缺接口抛错
- ``pairing.assemble``        配对结果落到 ``run.json`` 的接线（延迟可用这一支）
- ``shard.events``            分片的事件处理：失败轮只驱动一轮（网关同时发 turn_result 与 error）
- ``proc.sampling``           进程采样与 fd 软限抬升
- ``run.summary``             ``--json`` 一行摘要的字段与取值投影
- ``cli.contract``            冻结 CLI 的校验分支与默认值
- ``shapes.contract``         形态契约与剧本覆盖度、prompt 文本全局唯一
- ``report.render``           合成 ``run.json`` → markdown（缺数据是 ``—``，不可用有横幅）
- ``e2e``                     **真网关子进程 + 真分片子进程**跑完迷你一档，断言产物形状；
                               并用一个失败档断言「成功档与失败档的键集完全相同」

``e2e`` 通过注入的 ``run_ladder`` 复用正式路径——自测里跑的是产品代码，不是另一份平行
实现（否则自测绿了也不能说明工具能用）。
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

from wing_probe.provider.request_log import RequestLog

import cli
import driver_shard
import metrics
import orchestrator
import proc
import report
import shapes
from orchestrator import RunConfig, level_skeleton

#: e2e 一档的预算（秒）。自测总预算 ≤60 s，留足余量。
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


def _provider(frames: Mapping[int, Sequence[tuple[str, float]]]) -> Any:
    """合成 Provider：``{request_index: [(kind, at), …]}``，与上游 ``FrameEmit`` 同形。"""
    emits = [
        SimpleNamespace(request_index=index, frame_index=position, at=at, kind=kind)
        for index, values in frames.items()
        for position, (kind, at) in enumerate(values)
    ]
    return SimpleNamespace(frame_emits=tuple(emits))


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
    """按通道、按下标配对 + 各类丢弃原因 + 缺接口抛错。"""
    # 真实形态：meta 帧夹在内容帧之间（role 帧、finish 帧、usage 帧）。
    frames = {
        0: [
            ("meta", 100.0),
            ("content", 100.02),
            ("content", 100.04),
            ("content", 100.06),
            ("meta", 100.08),
        ],
        1: [("meta", 200.0), ("content", 200.02), ("content", 200.04)],
    }
    provider = _provider(frames)
    ledger = metrics.build_emit_ledger(provider)
    _check(
        ledger.kinds(0)["content"] == (100.02, 100.04, 100.06),
        f"meta frames must be excluded from the ledger: {ledger.kinds(0)}",
    )
    requests = [_request(0, "load:s0:0"), _request(1, "load:s1:0")]
    turns = [
        metrics.TurnSample(
            prompt="load:s0:0",
            sent_at=99.9,
            result_at=100.1,
            arrivals={"content": (100.021, 100.043, 100.064)},
        ),
        metrics.TurnSample(
            prompt="load:s1:0",
            sent_at=199.9,
            result_at=200.1,
            arrivals={"content": (200.022,)},
        ),
    ]
    pairing = metrics.pair_delivery(ledger, requests, turns)
    _check(pairing.paired == 4, f"expected 4 paired frames, got {pairing.paired}")
    _check(
        pairing.channels == {"content": 4},
        f"pairs must be attributed to their channel: {dict(pairing.channels)}",
    )
    _check(
        sorted(round(value, 3) for value in pairing.latencies_ms)
        == [1.0, 2.0, 3.0, 4.0],
        f"latency must be arrival - emit, got {pairing.latencies_ms}",
    )
    # 第 1 轮的第二个内容帧客户端没收到，但这轮是「完成」的 → 真丢事件，必须点名。
    _check(
        pairing.reasons.get("missing_events:content") == 1,
        f"a completed turn missing an event must be classified: {dict(pairing.reasons)}",
    )
    _check(
        sum(pairing.reasons.values()) == pairing.dropped,
        "drop reasons must add up to the dropped frame count",
    )

    # 通道各归各：thinking → reasoning、tool_call → tool_call_stream，互不干扰。
    mixed = _provider(
        {
            0: [
                ("meta", 100.0),
                ("thinking", 100.02),
                ("content", 100.04),
                ("tool_call", 100.06),
                ("meta", 100.08),
            ]
        }
    )
    mixed_turn = metrics.TurnSample(
        "p",
        99.0,
        101.0,
        {
            "thinking": (100.021,),
            "content": (100.043,),
            "tool_call": (100.062,),
        },
    )
    mixed_pairing = metrics.pair_delivery(
        metrics.build_emit_ledger(mixed), [_request(0, "p")], [mixed_turn]
    )
    _check(
        mixed_pairing.paired == 3
        and dict(mixed_pairing.channels)
        == {"content": 1, "thinking": 1, "tool_call": 1},
        f"each channel must pair on its own: {dict(mixed_pairing.channels)}",
    )

    # 窗口末仍在飞的那一轮：发射端多出的帧记 in_flight_tail（预期），不吞不猜。
    in_flight = metrics.pair_delivery(
        ledger,
        requests,
        [metrics.TurnSample("load:s1:0", 199.9, None, {"content": (200.022,)})],
    )
    _check(
        in_flight.paired == 1 and in_flight.reasons.get("in_flight_tail:content") == 1,
        f"an in-flight turn must report its tail: {dict(in_flight.reasons)}",
    )
    # 客户端多收了事件：同样按帧记账。
    unexpected = metrics.pair_delivery(
        ledger,
        requests,
        [
            metrics.TurnSample(
                "load:s1:0", 199.9, 200.1, {"content": (200.022, 200.044, 200.05)}
            )
        ],
    )
    _check(
        unexpected.reasons.get("unexpected_events:content") == 1,
        f"extra arrivals must be classified: {dict(unexpected.reasons)}",
    )
    # 窗口过滤：窗口外的配对不计入分布。
    windowed = metrics.pair_delivery(ledger, requests, turns, window=(100.03, 100.07))
    _check(
        windowed.reasons.get("outside_window") == 2 and windowed.paired == 2,
        f"pairs outside the window must be excluded: {dict(windowed.reasons)} "
        f"paired={windowed.paired}",
    )

    # 未知 prompt / 同一 prompt 命中多条（重试）→ 整轮丢弃，不猜。
    unknown = metrics.pair_delivery(
        ledger,
        requests,
        [metrics.TurnSample("load:zzz:0", 1.0, 2.0, {"content": (100.0, 100.1)})],
    )
    _check(unknown.paired == 0 and unknown.dropped == 2, "unknown prompt must drop")
    _check(
        unknown.reasons.get("unknown_prompt") == 2,
        "unknown prompt must be classified (frame counts)",
    )
    ambiguous = metrics.pair_delivery(
        ledger,
        [_request(0, "load:s0:0"), _request(1, "load:s0:0")],
        [
            metrics.TurnSample(
                "load:s0:0", 0.0, 1.0, {"content": (100.0, 100.02, 100.04)}
            )
        ],
    )
    _check(
        ambiguous.paired == 0 and ambiguous.dropped == 3, "ambiguous prompt must drop"
    )
    _check(
        ambiguous.reasons.get("ambiguous_prompt") == 3,
        "ambiguous prompt must be classified (frame counts)",
    )

    # 负延迟 = 错位，不写进样本。
    negative = metrics.pair_delivery(
        metrics.build_emit_ledger(
            _provider({0: [("content", 100.0), ("content", 100.02)]})
        ),
        [_request(0, "p")],
        [metrics.TurnSample("p", 0.0, 1.0, {"content": (99.9, 100.03)})],
    )
    _check(
        negative.paired == 1 and negative.dropped == 1,
        "negative latency must be dropped",
    )
    _check(
        negative.reasons.get("negative_latency") == 1,
        "negative latency must be classified",
    )

    # 缺接口必须显式失败（而不是返回空表 → 静默「无延迟数据」）。
    missing = SimpleNamespace()
    _check(not metrics.has_frame_emits(missing), "capability probe must report absence")
    try:
        metrics.build_emit_ledger(missing)
    except metrics.DeliveryUnavailable as exc:
        _check(
            "frame_emits not available" in str(exc),
            f"error text must be greppable: {exc}",
        )
    else:  # pragma: no cover - 红线回归
        raise SelftestFailure("missing frame_emits must raise DeliveryUnavailable")


def _sampling(**overrides: Any) -> orchestrator._Sampling:
    base: dict[str, Any] = {
        "window_start": 100.0,
        "window_end": 105.0,
        "wall_s": 5.0,
        "gateway_pid": 4242,
        "gateway_cpu_seconds": 0.5,
        "provider_cpu_seconds": 0.25,
        "rss_mb": [100.0, 104.0],
        "fds": [30, 40],
        "loadavg": [1.0, 1.0, 1.0],
    }
    base.update(overrides)
    return orchestrator._Sampling(**base)


def _sample_payload(**overrides: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "shard": 0,
        "text_frames": 3,
        "delta_frames": {"content": 3, "thinking": 0, "tool_call": 0},
        "turns_in_flight": 0,
        "errors": 0,
        "cpu_seconds": 0.5,
        "cpu_span_s": 5.0,
        "closed_before_end": 0,
        "error_details": [],
        "turns": [
            {
                "prompt": "load:s0:0",
                "sent_us": 1000,
                "result_us": 2001000,
                "arrivals_us": {"content": [16000, 27000, 38000]},
            }
        ],
    }
    base.update(overrides)
    return base


def check_assemble() -> None:
    """配对结果落到 ``run.json`` 的接线（延迟可用这一支，含丢弃与窗口不一致）。"""
    _check(
        set(level_skeleton(1)["metrics"]) == set(orchestrator.METRIC_KEYS),
        "the level skeleton must seed exactly the frozen metric keys",
    )
    requests = RequestLog()
    requests.record({"messages": [{"role": "user", "content": "load:s0:0"}]}, model="m")
    provider = SimpleNamespace(
        frame_emits=tuple(
            [
                SimpleNamespace(
                    request_index=0, frame_index=0, at=100.005, kind="meta"
                ),
                SimpleNamespace(
                    request_index=0, frame_index=1, at=100.015, kind="content"
                ),
                SimpleNamespace(
                    request_index=0, frame_index=2, at=100.025, kind="content"
                ),
                SimpleNamespace(
                    request_index=0, frame_index=3, at=100.035, kind="content"
                ),
                SimpleNamespace(
                    request_index=0, frame_index=4, at=100.045, kind="meta"
                ),
            ]
        )
    )
    provider.requests = requests
    cfg = RunConfig(
        scenario="pure_stream",
        levels=(1,),
        rate=50.0,
        dwell=5.0,
        warmup=0.0,
        shards=1,
        subscribers="per-shard",
        history_tokens=0,
        out=Path("/tmp/capacity-selftest-assemble"),
    )
    notes: list[str] = []
    record = orchestrator._assemble(
        level_skeleton(1),
        cfg=cfg,
        sessions=1,
        provider=provider,
        sampling=_sampling(),
        payloads=[_sample_payload()],
        notes=notes,
    )
    metrics_block = record["metrics"]
    samples = record["samples"]
    _check(samples["delivery_available"] is True, "delivery must be reported available")
    _check(samples["count"] == 3, f"expected 3 paired frames, got {samples['count']}")
    _check(samples["dropped"] == 0, f"expected 0 dropped, got {samples['dropped']}")
    _check(
        metrics_block["delivery.p50_ms"] == 2.0
        and metrics_block["delivery.p999_ms"] == 3.0,
        f"latency must land on the metric keys: {metrics_block}",
    )
    _check(
        record["metrics"]["gateway.cpu_cores"] == 0.1,
        f"gateway cpu = 0.5s / 5s: {record['metrics']['gateway.cpu_cores']}",
    )
    _check(
        record["rig"]["shard_cpu_cores"] == [0.1],
        f"shard cpu = 0.5s / 5s: {record['rig']['shard_cpu_cores']}",
    )
    _check(
        record["metrics"]["tokens_per_second_actual"] == 0.6
        and record["metrics"]["events_per_second_actual"] == 0.6,
        f"3 content frames / 5 s: {record['metrics']['tokens_per_second_actual']}"
        f" / {record['metrics']['events_per_second_actual']}",
    )
    _check(
        set(metrics_block) == set(orchestrator.METRIC_KEYS),
        f"assemble must not add or drop metric keys: "
        f"{sorted(set(metrics_block) ^ set(orchestrator.METRIC_KEYS))}",
    )
    _check(
        record["metrics"]["slow_client_recycles"] == 0,
        "no closed client must be reported as a recycle",
    )

    # 窗口末仍在飞的一轮：样本照样交出来（result_at 为 null），尾部记 in_flight_tail。
    in_flight_payload = _sample_payload(
        turns_in_flight=1,
        turns=[
            {
                "prompt": "load:s0:0",
                "sent_us": 1000,
                "result_us": None,
                "arrivals_us": {"content": [16000, 27000]},
            }
        ],
    )
    in_flight = orchestrator._assemble(
        level_skeleton(1),
        cfg=cfg,
        sessions=1,
        provider=provider,
        sampling=_sampling(),
        payloads=[in_flight_payload],
        notes=[],
    )
    _check(
        in_flight["samples"]["turns"] == 0
        and in_flight["samples"]["turns_in_flight"] == 1,
        f"an in-flight turn must be counted as such: {in_flight['samples']}",
    )
    _check(
        in_flight["samples"]["count"] == 2,
        f"an in-flight turn still yields deliveries: {in_flight['samples']['count']}",
    )
    _check(
        in_flight["metrics"]["turn.p50_ms"] is None,
        "no completed turn → turn.* is null (never 0)",
    )

    # 分片的记账区间与采样窗口不一致 → 必须留一条 note（而不是把两个窗口混着算）。
    mismatched: list[str] = []
    orchestrator._assemble(
        level_skeleton(1),
        cfg=cfg,
        sessions=1,
        provider=provider,
        sampling=_sampling(),
        payloads=[_sample_payload(cpu_span_s=2.0)],
        notes=mismatched,
    )
    _check(
        any("CPU window" in note for note in mismatched),
        f"a mismatched shard CPU window must be called out: {mismatched}",
    )

    # 客户端多收了一个事件：能配的照配，多出的按帧记账。
    short = _provider({0: [("content", 100.015), ("content", 100.025)]})
    short.requests = requests
    extra = orchestrator._assemble(
        level_skeleton(1),
        cfg=cfg,
        sessions=1,
        provider=short,
        sampling=_sampling(),
        payloads=[_sample_payload()],
        notes=[],
    )
    _check(
        extra["samples"]["count"] == 2 and extra["samples"]["dropped"] == 1,
        f"pairable frames must pair, the surplus must be dropped: {extra['samples']}",
    )
    _check(
        extra["samples"]["dropped_reasons"].get("unexpected_events:content") == 1,
        f"drop reason must be classified: {extra['samples']['dropped_reasons']}",
    )

    # 一个都配不上（prompt 不在请求留档里）→ 延迟是 null、count 是真实的 0。
    nothing = orchestrator._assemble(
        level_skeleton(1),
        cfg=cfg,
        sessions=1,
        provider=provider,
        sampling=_sampling(),
        payloads=[
            _sample_payload(
                turns=[
                    {
                        "prompt": "load:zzz:0",
                        "sent_us": 1000,
                        "result_us": 2001000,
                        "arrivals_us": {"content": [16000, 27000, 38000]},
                    }
                ]
            )
        ],
        notes=[],
    )
    _check(
        nothing["metrics"]["delivery.p50_ms"] is None,
        "no pairable frame must leave delivery null (never 0)",
    )
    _check(
        nothing["samples"]["count"] == 0 and nothing["samples"]["dropped"] == 3,
        f"unpairable arrivals must be counted: {nothing['samples']}",
    )
    _check(
        nothing["samples"]["dropped_reasons"].get("unknown_prompt") == 3,
        f"drop reason must be classified: {nothing['samples']['dropped_reasons']}",
    )


def check_shard_events() -> None:
    """分片的事件处理：一轮只驱动一轮、失败只计一次、窗口外不计。

    失败轮上网关**同时**发 ``turn_result(is_error)`` 与 ``error``；两者都当轮次边界会把
    同一轮驱动成两轮（并覆盖在飞轮次的起算点），两个都计数会把一次失败算成两次。
    """
    shape = shapes.resolve("pure_stream")
    config = driver_shard.ShardConfig(
        shard=0,
        gateway_url="http://127.0.0.1:1",
        sessions=("s0",),
        subscribers="per-shard",
        shape=shape.NAME,
        out=Path("/tmp/capacity-selftest-shard.json"),
    )
    shard = driver_shard.Shard(config, shape)
    connection = SimpleNamespace(closed=False)
    shard.states["s0"] = driver_shard._SessionState(session_id="s0", ws=connection)
    shard.open_window(100.0, 110.0)
    shard.start_cpu_accounting()
    while not shard._pending.empty():  # 首轮由 open_window 触发，这里当作 pump 已消费
        shard._pending.get_nowait()
    state = shard.states["s0"]
    state.prompt = "load:s0:0"
    state.sent_at = 101.0

    shard._on_event("s0", "text", {"content": "x"}, SimpleNamespace(at=101.5))
    shard._on_event("s0", "reasoning", {"content": "y"}, SimpleNamespace(at=101.6))
    shard._on_event("s0", "turn_result", {"is_error": True}, SimpleNamespace(at=103.0))
    shard._on_event("s0", "error", {"message": "boom"}, SimpleNamespace(at=103.0))
    shard._on_event("s0", "turn_result", {"is_error": True}, SimpleNamespace(at=103.0))
    _check(shard.errors == 1, f"one failed turn must count once, got {shard.errors}")
    _check(len(shard.turns) == 1, f"one turn sample expected, got {len(shard.turns)}")
    _check(
        shard.turns[0].arrivals == {"content": (101.5,), "thinking": (101.6,)},
        f"arrivals must be filed per channel: {dict(shard.turns[0].arrivals)}",
    )
    _check(
        shard.window_events["content"] == 1 and shard.window_events["thinking"] == 1,
        f"in-window events must be counted per channel: {shard.window_events}",
    )
    _check(
        shard._pending.qsize() == 1,
        f"a single turn must queue exactly one next prompt, got {shard._pending.qsize()}",
    )
    # 窗口外的事件既不计错误也不排队。
    state.sent_at = 111.0
    shard._on_event("s0", "turn_result", {"is_error": True}, SimpleNamespace(at=112.0))
    _check(shard.errors == 1, "errors must be window-gated")
    _check(
        shard._pending.qsize() == 1, "a turn ending after the window must not requeue"
    )
    # 窗口末仍在飞的一轮也必须交出来（单轮形态下这是唯一的一轮）。
    state.sent_at = 104.0
    state.arrivals["content"] = [104.5]
    shard.close_window()
    _check(
        len(shard.turns) == 2 and shard.turns[1].result_at is None,
        f"the in-flight turn must be flushed at window end: {shard.turns}",
    )
    _check(
        shard.turns[1].arrivals == {"content": (104.5,)},
        f"in-flight arrivals must be kept: {dict(shard.turns[1].arrivals)}",
    )
    payload = shard.payload()
    _check(
        payload["closed_before_end"] == 0,
        "an open connection must not count as a recycle",
    )
    _check(
        payload["cpu_span_s"] is not None,
        "CPU accounting must record its own window length",
    )
    _check(
        payload["turns_in_flight"] == 1,
        f"in-flight turns must be reported: {payload['turns_in_flight']}",
    )
    _check(
        payload["delta_frames"]["content"] == 1,
        f"in-window delta events must be per channel: {payload['delta_frames']}",
    )


def check_summary() -> None:
    """``--json`` 的一行摘要：字段名是下游脚本的契约，不能悄悄改。"""
    import run as run_module  # 函数级导入：run.py 只在自测分支里依赖本模块

    payload = _synthetic_run()
    payload["meta"]["out"] = "/tmp/capacity-summary"
    summary = run_module._summary(payload)
    for key in ("ok", "out", "run_json", "report", "levels", "failures"):
        _check(key in summary, f"--json summary is missing {key!r}")
    _check(
        summary["run_json"] == "/tmp/capacity-summary/run.json"
        and summary["report"] == "/tmp/capacity-summary/report.md",
        f"summary paths must be derived from meta.out: {summary}",
    )
    first = summary["levels"][0]
    for key in (
        "sessions",
        "gateway_cpu_cores",
        "delivery_p99_ms",
        "tokens_per_second_actual",
        "events_per_second_actual",
        "errors",
        "error",
    ):
        _check(key in first, f"--json level projection is missing {key!r}")
    _check(
        first["gateway_cpu_cores"]
        == payload["levels"][0]["metrics"]["gateway.cpu_cores"],
        "the projection must carry the metric value through",
    )
    _check(summary["ok"] is True and summary["failures"] == [], "healthy run summary")


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


# ── cli ───────────────────────────────────────────────────────


def check_cli() -> None:
    """冻结 CLI 的校验分支（下游脚本依赖这些名字与退出语义）。"""
    _check(
        cli.parse_levels(" 1, 10 ") == (1, 10),
        "levels must tolerate surrounding spaces",
    )
    for raw in ("10,1", "1,1", "1,x", "1,", "", "0", "-1"):
        try:
            cli.parse_levels(raw)
        except cli.UsageError:
            continue
        raise SelftestFailure(f"parse_levels({raw!r}) must raise UsageError")

    good = cli.build_config(
        cli.parse_args(
            [
                "--levels",
                "1,10",
                "--rate",
                "30",
                "--dwell",
                "2",
                "--warmup",
                "1",
                "--shards",
                "2",
                "--subscribers",
                "per-shard",
                "--out",
                "/tmp/capacity-cli-check",
            ]
        )
    )
    _check(
        good.levels == (1, 10) and good.rate == 30.0, "explicit args must be honoured"
    )
    _check(good.subscribers == "per-shard", "subscribers must round-trip")
    _check(good.shards_for(1) == 1 and good.shards_for(10) == 2, "shard clamping")

    _check(
        cli.build_config(cli.parse_args([])).out.is_absolute(),
        "the default --out must be resolved to an absolute path",
    )
    _check(
        cli.build_config(cli.parse_args(["--out", "target/x"])).out.is_absolute(),
        "a relative --out must be resolved to an absolute path",
    )
    defaults = cli.build_config(cli.parse_args(["--out", "/tmp/capacity-cli-default"]))
    _check(
        defaults.levels == cli.parse_levels(cli.DEFAULT_LEVELS)
        and defaults.rate == cli.DEFAULT_RATE
        and defaults.dwell == cli.DEFAULT_DWELL,
        "documented defaults must come from cli",
    )

    for argv in (
        ["--history-tokens", "100"],
        ["--shards", "0"],
        ["--rate", "0"],
        ["--dwell", "0"],
        ["--warmup", "-1"],
        ["--levels", "10,1"],
        ["--levels", "0"],
    ):
        try:
            cli.build_config(cli.parse_args(argv))
        except cli.UsageError:
            continue
        raise SelftestFailure(f"build_config({argv}) must raise UsageError")
    try:
        cli.build_config(cli.parse_args(["--scenario", "nope"]))
    except shapes.ShapeError as exc:
        _check("pure_stream" in str(exc), f"unknown scenario must list choices: {exc}")
    else:  # pragma: no cover - 注册表回归
        raise SelftestFailure("unknown --scenario must raise ShapeError")


# ── shapes ────────────────────────────────────────────────────


def check_shapes() -> None:
    """形态契约：注册表能解析，剧本覆盖整档所需轮次，prompt 文本全局唯一。"""
    shape = shapes.resolve("pure_stream")
    sessions, rate, span = 3, 20.0, 6.0
    tokens = shape.turn_tokens(rate, span)
    _check(
        tokens >= rate * span,
        f"one turn must cover the whole span: {tokens} token(s) < {rate * span}",
    )
    _check(
        shape.turn_tokens(rate, span) > shape.turn_tokens(rate, span / 2),
        "turn length must follow the span",
    )
    script = shape.script(sessions=sessions, rate=rate, span_s=span)
    _check(
        len(script) >= sessions,
        f"script must hold at least one turn per session, got {len(script)}",
    )
    _check(
        script.turns[0].text is not None
        and len(script.turns[0].text) == tokens
        and script.turns[0].chunk == 1,
        "the turn must stream 1 token per frame over the whole span",
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


# ── report ────────────────────────────────────────────────────


def _synthetic_run() -> dict[str, Any]:
    """两档合成产物：一档齐备、一档延迟不可用（两者键集相同）。"""
    meta = {
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
        "host": {"cpus": 12, "mem_gb": 16.0, "platform": "darwin", "ulimit_n": 4096},
        "gateway": {"pid": 1, "version": "test", "commit": "abcdef0"},
        "session_create": {"ok": 11, "failed": 0, "total_ms": 12.0},
        "notes": [],
        "failures": [],
    }
    first = level_skeleton(1)
    first["wall_s"] = 5.0
    first["metrics"].update(
        {
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
        }
    )
    first["rig"].update(
        {
            "provider_cpu_cores": 0.2,
            "shard_cpu_cores": [0.1],
            "host_loadavg": [1.0, 1.0, 1.0],
        }
    )
    first["samples"].update(
        {
            "count": 250,
            "dropped": 0,
            "dropped_reasons": {},
            "text_frames": 250,
            "turns": 1,
            "delivery_available": True,
        }
    )
    first["session_create"].update({"ok": 1, "failed": 0, "total_ms": 4.0})
    second = level_skeleton(10)
    second["wall_s"] = 5.0
    second["metrics"].update(
        {
            "gateway.cpu_cores": 1.0,
            "gateway.rss_mb_peak": 200.0,
            "gateway.rss_mb_end": 202.0,
            "gateway.fds_peak": 300,
            "tokens_per_second_actual": 500.0,
            "errors": 2,
            "slow_client_recycles": 1,
        }
    )
    second["rig"].update(
        {"provider_cpu_cores": 0.5, "shard_cpu_cores": [0.4, 0.3], "host_loadavg": None}
    )
    second["samples"].update(
        {
            "count": None,
            "dropped": None,
            "text_frames": 2500,
            "turns": 10,
            "delivery_available": False,
        }
    )
    second["session_create"].update({"ok": 10, "failed": 0, "total_ms": 8.0})
    return {"meta": meta, "levels": [first, second]}


def check_report() -> None:
    """合成 run.json → markdown：列数自洽、缺数据占位、不可用横幅、多行 note 折行。"""
    run = _synthetic_run()
    run["levels"][1]["notes"] = ["multi\nline\ndetail"]
    text = report.render(run)
    _check(
        text.startswith("# capacity — pure_stream"), "report must start with its title"
    )
    _check("| 1 |" in text and "| 10 |" in text, "each level must get a row")
    _check("| 0.10 |" in text, "gateway cpu must be rendered with two decimals")
    _check("投递延迟不可用" in text, "unavailable delivery must be called out up front")
    _check("— / —" in text, "missing turn metrics must render as an em dash, not 0")
    _check("nearest-rank" in text, "report must state the percentile basis")
    _check("0.40, 0.30" in text, "per-shard CPU must be listed (rig quality signal)")
    for line in text.splitlines():
        if line.startswith("| N |"):
            header = len(line.split("|")) - 2
        elif line.startswith("|---") and "N" in text:
            separator = len(line.split("|")) - 2
            _check(
                separator == header,
                f"table separator has {separator} cells but the header has {header} "
                "(GFM drops mismatched tables)",
            )
    _check(
        "- N=10: multi line detail" in text,
        "multi-line notes must fold onto one line inside the list item",
    )
    line = report.summary_line(run["levels"][0])
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
    """迷你一档走真实路径：真网关子进程 + 真分片子进程 + 真产物 + 失败档形状。"""
    proc.raise_nofile(proc.nofile_target(2))  # 2 条会话 + 网关自己的 fd，别撞软限
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
        _check(level.get("error") is None, f"level failed: {level.get('error')}")

        _check(level["sessions"] == 2, "level must report the requested N")
        _check(level["session_create"]["ok"] == 2, "both sessions must be created")
        metrics_block = level["metrics"]
        for key in orchestrator.METRIC_KEYS:
            _check(key in metrics_block, f"missing frozen metric key {key!r}")
        for key in orchestrator.SAMPLE_KEYS:
            _check(key in level["samples"], f"missing sample key {key!r}")
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
        samples = level["samples"]
        _check(samples["text_frames"] > 0, "text frames must be counted")
        _check(
            samples["delta_frames"]["content"] > 0,
            f"delta frames must be counted per channel: {samples['delta_frames']}",
        )
        # 单轮形态：轮长覆盖 warmup+dwell，所以窗口内没有完成的轮次，但那一轮仍在飞
        # 且它的到达时刻必须被交出来（否则投递延迟无从配对）。
        _check(
            samples["turns"] == 0 and samples["turns_in_flight"] >= 1,
            f"pure_stream is single-turn: {samples['turns']} completed, "
            f"{samples['turns_in_flight']} in flight",
        )
        _check(
            metrics_block["turn.p50_ms"] is None,
            "no completed turn must leave turn.* null (never 0)",
        )
        _check(
            any(
                "no turn completed inside the window" in note for note in level["notes"]
            ),
            f"the single-turn shape must explain the null turn metric: {level['notes']}",
        )
        _check(
            level["rig"]["shard_cpu_cores"],
            "shard CPU must be reported (rig quality signal)",
        )
        _check(
            metrics_block["slow_client_recycles"] == 0,
            "a healthy window must not count the driver's own teardown as a recycle: "
            f"{metrics_block['slow_client_recycles']}",
        )
        span_note = [note for note in level["notes"] if "CPU window" in note]
        _check(
            not span_note,
            f"shard CPU window must match the sample window: {span_note}",
        )

        # 投递延迟：上游接口在这个树上就位，所以必须**有值**，而且每个窗口内的内容
        # 增量事件都要配上（配对是按通道、按下标的，条数必须闭合）。
        _check(
            samples["delivery_available"] is True,
            f"the emitter clock is part of the tree, delivery must be available: "
            f"{samples['delivery_available']}",
        )
        p50 = metrics_block["delivery.p50_ms"]
        p99 = metrics_block["delivery.p99_ms"]
        _check(
            p50 is not None and p99 is not None, "available delivery must have values"
        )
        _check(0.0 <= p50 <= p99, f"latency order violated: p50={p50} p99={p99}")
        _check(
            samples["count"] == samples["delta_frames"]["content"],
            f"every in-window content event must pair: paired={samples['count']} "
            f"content frames={samples['delta_frames']['content']} "
            f"reasons={samples['dropped_reasons']}",
        )
        unexpected = {
            reason: count
            for reason, count in (samples["dropped_reasons"] or {}).items()
            if reason.startswith(("missing_events", "unexpected_events", "negative"))
        }
        _check(
            not unexpected,
            f"channel pairing must close on this shape: {unexpected}",
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
        _check(
            not (out / "raw" / "2" / "workspace").exists(),
            "a completed level must not leave its scratch workspace behind",
        )
        shard = json.loads(shard_file.read_text(encoding="utf-8"))
        _check(shard["shard"] == 0, "shard sample file must name its shard")
        _check(
            isinstance(shard["window"], dict)
            and shard["window"]["end"] > shard["window"]["start"],
            "shard sample file must carry a window",
        )
        _check(len(shard["turns"]) >= 1, "shard must report at least one turn sample")

        check_relative_out(run_ladder, Path(tmp))
        check_failure_shape(run_ladder, Path(tmp) / "failed", level)


def check_relative_out(run_ladder: LadderRunner, tmp: Path) -> None:
    """``--out`` 相对路径（也是默认形态）必须能跑完：分片以档位目录为 cwd 起。"""
    previous = Path.cwd()
    os.chdir(tmp)
    try:
        cfg = _e2e_config(Path("relative-run"))
        payload = asyncio.run(run_ladder(cfg, selftest_run=True))
    finally:
        os.chdir(previous)
    _check(
        payload["meta"]["failures"] == [],
        f"relative --out failed: {payload['meta']['failures']}",
    )
    _check(
        Path(payload["meta"]["out"]).is_absolute(),
        f"meta.out must be absolute: {payload['meta']['out']}",
    )
    _check(
        (tmp / "relative-run" / "run.json").is_file(),
        "relative --out must write run.json next to the cwd",
    )


def check_failure_shape(
    run_ladder: LadderRunner, out: Path, good: Mapping[str, Any]
) -> None:
    """失败档的键集必须与成功档完全相同（缺数据是 ``null``，不是缺键）。"""
    previous = os.environ.get("WING_GATEWAY_BIN")
    os.environ["WING_GATEWAY_BIN"] = str(out / "no-such-gateway")
    try:
        payload = asyncio.run(run_ladder(_e2e_config(out), selftest_run=True))
    finally:
        if previous is None:
            os.environ.pop("WING_GATEWAY_BIN", None)
        else:
            os.environ["WING_GATEWAY_BIN"] = previous
    _check(payload["meta"]["failures"], "a level that cannot start must be reported")
    bad = payload["levels"][0]
    _check(bad.get("error"), f"failed level must carry an error: {bad.get('error')}")
    _check(
        set(bad) == set(good),
        f"failed level keys differ from a good one: {sorted(set(bad) ^ set(good))}",
    )
    for block in ("metrics", "samples", "gateway", "rig", "session_create", "shape"):
        _check(
            set(bad[block]) == set(good[block]),
            f"failed level {block} keys differ: "
            f"{sorted(set(bad[block]) ^ set(good[block]))}",
        )
    _check(
        all(value is None for value in bad["metrics"].values()),
        "failed level metrics must be null, not 0",
    )
    _check(
        payload["meta"]["session_create"]["ok"] is None,
        "session_create must not sum a failed level's missing counts as 0",
    )


# ── 入口 ──────────────────────────────────────────────────────


def checks() -> list[tuple[str, Callable[[], None]]]:
    return [
        ("stats.percentile", check_stats),
        ("pairing.last_user_text", check_last_user_text),
        ("pairing.delivery", check_pairing),
        ("pairing.assemble", check_assemble),
        ("shard.events", check_shard_events),
        ("proc.sampling", check_proc),
        ("run.summary", check_summary),
        ("cli.contract", check_cli),
        ("shapes.contract", check_shapes),
        ("report.render", check_report),
    ]


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
