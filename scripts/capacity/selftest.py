#!/usr/bin/env python3
"""离线自测（``run.py --selftest``）：≤120 秒、不联网、不需要 API key、可重复。

十二个检查（见 :func:`checks`）+ 负载形态各一次 ``e2e``，对应这个工具必须可信的几件事：

- ``stats.percentile``        百分位秩定义（known answer，含 n=1 / 偶数 / 极值 / 空序列报错）
- ``pairing.last_user_text``  配对键的提取（str / block 列表两种 body 形态）
- ``pairing.delivery``        按下标配对 + 丢弃原因 + 同一 prompt 多请求时按通道消歧
- ``pairing.assemble``        配对结果落到 ``run.json`` 的接线（延迟可用这一支）
- ``shard.events``            分片的事件处理：失败轮只驱动一轮、字符实测、水合两段时间线
- ``proc.sampling``           进程采样与 fd 软限抬升
- ``run.summary``             ``--json`` 一行摘要的字段与取值投影
- ``cli.contract``            冻结 CLI 的校验分支与默认值（含形态专属旋钮的适用范围）
- ``shapes.contract``         五种形态的 plan 契约：model / 会话归属 / chunk / 窗口模式
- ``verdict.criteria``        判据：baseline / degraded / capped / unknown / low_confidence
- ``verdict.polluted``        被污染的 ``run.json`` 必须被标低置信（负向验证）
- ``report.render``           合成 ``run.json`` → markdown（缺数据是 ``—``、判据原文唯一）
- ``e2e.<case>``              **真网关子进程 + 真分片子进程**跑完迷你一档（各形态各一次 + `--tool bash` 变体），
                               断言该形态的指标非 null 且形状自洽；另用失败档断言
                               「成功档与失败档的键集完全相同」

``e2e`` 通过注入的 ``run_ladder`` 复用正式路径——自测里跑的是产品代码，不是另一份平行
实现（否则自测绿了也不能说明工具能用）。
"""

from __future__ import annotations

import asyncio
import inspect
import json
import os
import sys
import tempfile
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import httpx
from wing_probe.provider.request_log import RequestLog

import cli
import driver_shard
import metrics
import orchestrator
import proc
import report
import shape_mixed
import shapes
import verdict
from orchestrator import RunConfig, level_skeleton

#: 每个 e2e 档位的预算（秒）。自测总预算 ≤120 s，五种形态各一档也够。
E2E_BUDGET_S = 45.0

#: e2e 的会话数（两种形态/两组的边界都覆盖得到）。
E2E_SESSIONS = 2

#: e2e 用的池上限（不为限制任何东西，只为证明旗标真的落进了生成配置）。
E2E_POOL_CAP = 7

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

    # 工具轮的两段式：同一个 prompt 触发两次请求（调用轮 + 文本轮），按通道消歧。
    two_requests = _provider(
        {
            0: [("tool_call", 100.02)],
            1: [("meta", 100.5), ("content", 100.52)],
        }
    )
    cycle_turn = metrics.TurnSample(
        "load:s0:0", 99.9, 100.6, {"tool_call": (100.021,), "content": (100.522,)}
    )
    cycle_pairing = metrics.pair_delivery(
        metrics.build_emit_ledger(two_requests),
        [_request(0, "load:s0:0"), _request(1, "load:s0:0")],
        [cycle_turn],
    )
    _check(
        cycle_pairing.paired == 2
        and dict(cycle_pairing.channels) == {"content": 1, "tool_call": 1},
        f"two requests per prompt must disambiguate per channel: "
        f"{dict(cycle_pairing.channels)} reasons={dict(cycle_pairing.reasons)}",
    )
    _check(
        not cycle_pairing.reasons,
        f"a clean two-request cycle must drop nothing: {dict(cycle_pairing.reasons)}",
    )
    # 两个请求都发过同一通道的帧 = 真重试，无法归因 → 丢弃。
    retried = metrics.pair_delivery(
        metrics.build_emit_ledger(
            _provider({0: [("content", 100.02)], 1: [("content", 100.5)]})
        ),
        [_request(0, "load:s0:0"), _request(1, "load:s0:0")],
        [metrics.TurnSample("load:s0:0", 99.9, 100.6, {"content": (100.52,)})],
    )
    _check(
        retried.paired == 0 and retried.reasons.get("ambiguous_prompt") == 1,
        f"a real retry must stay ambiguous: {dict(retried.reasons)}",
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
        "chars": 3,
        "sync_sessions": 0,
        "hydration_attempts": 0,
        "hydration_failed": 0,
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
                "hydrate_sent_us": None,
                "sync_us": None,
                "arrivals_us": {"content": [16000, 27000, 38000]},
            }
        ],
    }
    base.update(overrides)
    return base


def _assemble_cfg(**overrides: Any) -> RunConfig:
    base: dict[str, Any] = {
        "scenario": "pure_stream",
        "levels": (1,),
        "rate": 50.0,
        "dwell": 5.0,
        "warmup": 0.0,
        "shards": 1,
        "subscribers": "per-shard",
        "history_tokens": 0,
        "out": Path("/tmp/capacity-selftest-assemble"),
    }
    base.update(overrides)
    return RunConfig(**base)


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
    cfg = _assemble_cfg()
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
        record["metrics"]["chars_per_second"] == 0.6,
        f"chars are measured, not derived: {record['metrics']['chars_per_second']}",
    )
    _check(
        record["metrics"]["turns_per_second"] == 0.2,
        f"one completed turn / 5 s: {record['metrics']['turns_per_second']}",
    )
    _check(
        metrics_block["hydration.p50_ms"] is None
        and metrics_block["first_frame.p50_ms"] is None,
        f"no hydration happened, so those keys stay null: {metrics_block}",
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
                "hydrate_sent_us": None,
                "sync_us": None,
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
                        "hydrate_sent_us": None,
                        "sync_us": None,
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

    # 水合两段时间线：订阅发出 → sync 到达（水合），prompt 发出 → 首帧（驱动）。
    hydrated_payload = _sample_payload(
        sync_sessions=1,
        hydration_attempts=1,
        turns=[
            {
                "prompt": "load:s0:0",
                "sent_us": 300000,
                "result_us": 2001000,
                "hydrate_sent_us": 0,
                "sync_us": 120000,
                "arrivals_us": {"content": [360000, 470000, 580000]},
            }
        ],
    )
    hydrated = orchestrator._assemble(
        level_skeleton(1),
        cfg=_assemble_cfg(scenario="hydration"),
        sessions=1,
        provider=provider,
        sampling=_sampling(),
        payloads=[hydrated_payload],
        notes=[],
    )
    _check(
        hydrated["metrics"]["hydration.p50_ms"] == 120.0,
        f"hydration = sync arrival - subscribe: {hydrated['metrics']}",
    )
    _check(
        hydrated["metrics"]["first_frame.p50_ms"] == 60.0,
        f"first frame = first content arrival - prompt: {hydrated['metrics']}",
    )
    _check(
        hydrated["samples"]["sync_sessions"] == 1
        and hydrated["samples"]["hydration_attempts"] == 1,
        f"hydration counters must land on samples: {hydrated['samples']}",
    )


def check_shard_events() -> None:
    """分片的事件处理：一轮只驱动一轮、失败只计一次、窗口外不计、字符实测、水合两段。

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
    connection = SimpleNamespace(closed=False, client_id="c0", frames_received=0)
    shard.states["s0"] = driver_shard._SessionState(session_id="s0", ws=connection)
    shard.open_window(100.0, 110.0)
    shard.start_cpu_accounting()
    while not shard._pending.empty():  # 首轮由 open_window 触发，这里当作 pump 已消费
        shard._pending.get_nowait()
    state = shard.states["s0"]
    state.prompt = "load:s0:0"
    state.sent_at = 101.0

    shard._on_event("s0", "text", {"content": "xyz"}, SimpleNamespace(at=101.5))
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
        shard.window_chars == 3,
        f"chars must be measured from the text payload, got {shard.window_chars}",
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
    metrics.check_shard_payload(payload)

    # 水合形态：sync 到达才算水合完成，之后才驱动一轮，且不再补发。
    hydrate_config = driver_shard.ShardConfig(
        shard=0,
        gateway_url="http://127.0.0.1:1",
        sessions=("s0",),
        subscribers="per-shard",
        shape="hydration",
        out=Path("/tmp/capacity-selftest-shard-hydrate.json"),
        mode="hydrate",
    )
    hydrate = driver_shard.Shard(hydrate_config, shapes.resolve("hydration"))
    hydrate.states["s0"] = driver_shard._SessionState(session_id="s0", ws=connection)
    hydrate.open_window(200.0, 210.0)
    _check(
        hydrate._pending.empty(),
        "hydrate mode must not queue prompts at window open (they follow sync_session)",
    )
    state = hydrate.states["s0"]
    state.hydrate_sent_at = 200.1
    hydrate._on_event("s0", "sync_session", {}, SimpleNamespace(at=200.9))
    _check(
        state.hydrated_at == 200.9 and hydrate.sync_sessions == 1,
        f"sync_session must close the hydration interval: {state.hydrated_at}",
    )
    _check(
        hydrate._pending.qsize() == 1,
        "the prompt must be queued only after sync_session arrives",
    )
    hydrate._pending.get_nowait()  # 轮次泵取走它并发出（这里直接模拟已发出）
    state.prompt = "load:s0:0"
    state.sent_at = 201.0
    hydrate._on_event("s0", "text", {"content": "x"}, SimpleNamespace(at=201.2))
    hydrate._on_event("s0", "turn_result", {}, SimpleNamespace(at=201.3))
    _check(
        len(hydrate.turns) == 1 and hydrate.turns[0].hydrated_at == 200.9,
        f"the hydration interval must ride along with the turn sample: {hydrate.turns}",
    )
    _check(
        hydrate._pending.empty(),
        "hydrate mode drives exactly one turn per session (no re-queue)",
    )


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
                "--chunk",
                "4",
                "--dwell",
                "2",
                "--warmup",
                "1",
                "--shards",
                "2",
                "--subscribers",
                "per-shard",
                "--backend",
                "memory",
                "--out",
                "/tmp/capacity-cli-check",
            ]
        )
    )
    _check(
        good.levels == (1, 10) and good.rate == 30.0, "explicit args must be honoured"
    )
    _check(good.subscribers == "per-shard", "subscribers must round-trip")
    _check(good.chunk == 4, "--chunk must round-trip")
    _check(good.backend == "memory", "--backend must round-trip")
    _check(good.shards_for(1) == 1 and good.shards_for(10) == 2, "shard clamping")
    _check(
        good.provider_max_connections is None,
        "the provider pool cap must stay unset unless asked (generated config is "
        "byte-identical by default)",
    )
    _check(
        cli.build_config(
            cli.parse_args(["--provider-max-connections", "2000", "--out", "/tmp/x"])
        ).provider_max_connections
        == 2000,
        "--provider-max-connections must reach the config",
    )

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
        and defaults.dwell == cli.DEFAULT_DWELL
        and defaults.chunk == shapes.DEFAULT_CHUNK == 1
        and defaults.backend == "file"
        and defaults.tool == shapes.DEFAULT_TOOL
        and defaults.cycle_s == shapes.DEFAULT_CYCLE_S
        and defaults.decode_duty == shapes.DEFAULT_DECODE_DUTY
        and defaults.provider_max_connections is None,
        "documented defaults must come from cli / shapes",
    )

    for argv in (
        ["--shards", "0"],
        ["--rate", "0"],
        ["--chunk", "0"],
        ["--dwell", "0"],
        ["--warmup", "-1"],
        ["--levels", "10,1"],
        ["--levels", "0"],
        ["--history-tokens", "-1"],
        ["--cycle-s", "-0.5"],
        ["--decode-duty", "1.5"],
        # 形态不消费的旋钮不许显式给非默认值（静默忽略会让人以为测了那个变量）。
        ["--history-tokens", "100"],
        ["--tool", "bash"],
        ["--cycle-s", "1"],
        # 水合形态与内存后端互斥（进程内会话不过重启）。
        ["--scenario", "hydration", "--backend", "memory"],
        # 池上限必须正整数：0 / 负数都不是「无上限」的合法写法。
        ["--provider-max-connections", "0"],
        ["--provider-max-connections", "-5"],
        # 形态自己的参数校验也要在起进程之前变成退出码 2。
        ["--scenario", "tool_cycle", "--tool", "nope"],
    ):
        try:
            cli.build_config(cli.parse_args(["--out", "/tmp/x", *argv]))
        except cli.UsageError:
            continue
        raise SelftestFailure(f"build_config({argv}) must raise UsageError")
    # 旗标名只有一张表：argparse 的默认值来自它，报告里的复现命令也由它的键派生。
    specs = {spec.dest: spec for spec in cli.ARG_SPECS}
    parsed = cli.parse_args([])
    for dest, spec in specs.items():
        _check(
            getattr(parsed, dest) == spec.default,
            f"the parser default of --{dest} must come from ARG_SPECS: "
            f"{getattr(parsed, dest)!r} != {spec.default!r}",
        )
        _check(
            spec.dest == spec.flag.lstrip("-").replace("-", "_"),
            f"a flag's dest must be derivable from its name: {spec.flag}",
        )
    cfg = cli.build_config(
        cli.parse_args(
            [
                "--scenario",
                "mixed",
                "--levels",
                "1,10",
                "--chunk",
                "4",
                "--cycle-s",
                "0.2",
                "--decode-duty",
                "0.5",
                "--backend",
                "memory",
                "--out",
                "/tmp/capacity-repro",
            ]
        )
    )
    command = report.render(
        {
            "meta": {"scenario": cfg.scenario, "args": cli.measurement_args(cfg)},
            "levels": [],
        }
    )
    line = next(
        text for text in command.splitlines() if text.startswith("uv run python")
    )
    _check(
        line.split()[4:] == _reproduce_argv(cfg),
        f"the reproduce command must round-trip the args: {line}",
    )
    # ``--out`` 是现场参数（不进 meta.args），所以比的是**测量参数**的往返。
    _check(
        cli.measurement_args(cli.build_config(cli.parse_args(line.split()[4:])))
        == cli.measurement_args(cfg),
        "the reproduce command must re-build the same measurement args",
    )
    try:
        cli.build_config(cli.parse_args(["--scenario", "nope", "--out", "/tmp/x"]))
    except shapes.ShapeError as exc:
        _check("pure_stream" in str(exc), f"unknown scenario must list choices: {exc}")
    else:  # pragma: no cover - 注册表回归
        raise SelftestFailure("unknown --scenario must raise ShapeError")


# ── shapes ────────────────────────────────────────────────────


def _shape_args(name: str, **overrides: Any) -> shapes.ShapeArgs:
    base: dict[str, Any] = {
        "scenario": name,
        "sessions": 3,
        "rate": 20.0,
        "span_s": 6.0,
        "dwell_s": 4.0,
        "chunk": 1,
        "cycle_s": shapes.DEFAULT_CYCLE_S,
        "decode_duty": shapes.DEFAULT_DECODE_DUTY,
        "tool": shapes.DEFAULT_TOOL,
        "history_tokens": 0,
        "frame_delay_s": 0.017,
    }
    base.update(overrides)
    return shapes.ShapeArgs(**base)


def _reproduce_argv(cfg: RunConfig) -> list[str]:
    """复现命令的期望 argv（顺序由 ``cli.ARG_SPECS`` 决定）。"""
    argv = ["--scenario", cfg.scenario]
    for spec in cli.ARG_SPECS:
        if spec.mode or not spec.echo or spec.dest == "scenario":
            continue
        value = getattr(cfg, spec.dest)
        if value is None:
            continue  # 与 report._reproduce 同口径：未设置的旗标不写进命令
        if spec.dest == "levels":
            value = ",".join(str(level) for level in value)
        argv.extend([spec.flag, str(value)])
    return argv


def check_shapes() -> None:
    """五种形态的 plan 契约：注册表能解析、会话 model 归属自洽、chunk 与窗口模式生效。"""
    for name in shapes.names():
        shape = shapes.resolve(name)
        plan = shapes.build_plan(_shape_args(name))
        _check(
            len(plan.session_models) == 3,
            f"{name}: session_models must cover every session",
        )
        declared = {model_id for model_id, _ in plan.models}
        _check(
            set(plan.session_models) <= declared,
            f"{name}: session models must be declared ({plan.session_models})",
        )
        _check(plan.tools, f"{name}: a shape must declare its tool set")
        prompt_text = shapes.prompt_text_of(shape)
        first = prompt_text("sess-a", 0)
        _check(first != prompt_text("sess-a", 1), f"{name}: prompt varies by turn")
        _check(first != prompt_text("sess-b", 0), f"{name}: prompt varies by session")
        _check(
            first != shapes.seed_key("sess-a", 0),
            f"{name}: seed prompts must never collide with driving prompts",
        )

    # pure_stream：一轮覆盖整窗、帧大小 = chunk。
    args = _shape_args("pure_stream", chunk=4)
    plan = shapes.build_plan(args)
    script = plan.models[0][1]
    turn = script.turns[0]
    _check(
        turn.chunk == 4 and len(turn.text or "") == args.chars_for(6.0 + 1.0),
        f"pure_stream: one turn must cover the span at --chunk chars per event: "
        f"chunk={turn.chunk} chars={len(turn.text or '')}",
    )
    _check(
        len(script) >= 3,
        f"pure_stream: the script must hold at least one turn per session: {len(script)}",
    )

    # tool_cycle：每会话一个 model（工具轮两段式不能共享游标）、循环剧本、workspace 文件。
    read_plan = shapes.build_plan(_shape_args("tool_cycle"))
    _check(
        len(set(read_plan.session_models)) == 3,
        f"tool_cycle: every session needs its own script cursor: "
        f"{read_plan.session_models}",
    )
    _check(
        "note.txt" in read_plan.workspace_files,
        "tool_cycle(read): the Read target must exist in the workspace",
    )
    cycle = dict(read_plan.models)[read_plan.session_models[0]]
    patterns = [cycle.consume()[0].describe() for _ in range(3)]
    _check(
        "Read" in patterns[0]
        and "tool_calls" in patterns[0]
        and "Read" not in patterns[1],
        f"tool_cycle: the cycle must alternate call / reply turns: {patterns}",
    )
    bash_plan = shapes.build_plan(_shape_args("tool_cycle", tool="bash"))
    _check(
        bash_plan.tools == ("Bash",),
        f"tool_cycle(bash): the tool set must follow --tool: {bash_plan.tools}",
    )
    _check(
        bash_plan.agent.get("yolo") is True,
        "tool_cycle(bash): headless runs have nobody to approve the command (yolo)",
    )

    # idle：不订阅（否则逐出永远不触发）、把逐出阈值压进窗口、声明逐出探针。
    idle_plan = shapes.build_plan(_shape_args("idle"))
    _check(
        idle_plan.window_mode == "hold" and not idle_plan.subscribe,
        f"idle: hold mode must not subscribe: {idle_plan.window_mode}",
    )
    # 空闲计时从建会话起算 ⇒ 「窗口内逐出」要求 warmup < ttl ≤ warmup + dwell。
    for span, dwell in ((4.0, 4.0), (600.0, 4.0), (3.0, 2.0)):
        ttl = float(
            (
                (
                    shapes.build_plan(
                        _shape_args("idle", span_s=span, dwell_s=dwell)
                    ).env_sessions
                    or {}
                ).get("eviction")
                or {}
            ).get("idle_ttl_seconds", 0)
        )
        warmup = span - dwell
        _check(
            warmup < ttl <= warmup + dwell,
            f"idle: the eviction TTL must land inside the window "
            f"(warmup={warmup}, dwell={dwell}, ttl={ttl})",
        )
    _check(idle_plan.measure_eviction, "idle: eviction must be probed after the window")

    # hydration：订阅发生在窗口里、采样更密。
    hydration_plan = shapes.build_plan(_shape_args("hydration"))
    _check(
        hydration_plan.window_mode == "hydrate" and hydration_plan.subscribe,
        f"hydration: hydrate mode needs subscriptions in the window: "
        f"{hydration_plan.window_mode}",
    )
    _check(
        hydration_plan.sample_interval_s < 1.0,
        f"hydration: the sampling interval must be finer than 1 s: "
        f"{hydration_plan.sample_interval_s}",
    )

    # mixed：按 decode-duty 分组，两组 model 不同。
    mixed_plan = shapes.build_plan(_shape_args("mixed", decode_duty=0.5))
    _check(
        len(set(mixed_plan.session_models)) == 2,
        f"mixed: 3 sessions at duty 0.5 must split into two model groups: "
        f"{mixed_plan.session_models}",
    )
    all_stream = shapes.build_plan(_shape_args("mixed", decode_duty=1.0))
    _check(
        len(set(all_stream.session_models)) == 1,
        f"mixed: duty 1.0 is all streaming: {all_stream.session_models}",
    )
    for sessions in (2, 3, 10, 100):
        streaming = shape_mixed.streaming_sessions(sessions, 0.7)
        _check(
            0 < streaming < sessions,
            f"mixed: {sessions} sessions must split into two non-empty groups, "
            f"got {streaming}",
        )
    _check(
        shape_mixed.streaming_sessions(1, 0.7) == 0,
        "mixed: a single session must land in the tool group (the turn baseline "
        "needs a completed turn)",
    )
    _check(
        shape_mixed.streaming_sessions(4, 0.0) == 0
        and shape_mixed.streaming_sessions(4, 1.0) == 4,
        "mixed: duty 0 / 1 must keep their literal meaning (all tool / all streaming)",
    )

    # 历史预置：只有声明了 --history-tokens 的形态接得住它，且剧本是循环的（会话数决定轮数）。
    seeded = shapes.build_plan(_shape_args("hydration", history_tokens=2000))
    _check(
        seeded.seed is not None
        and seeded.seed.chars_per_session >= 2000 * shapes.HISTORY_CHARS_PER_TOKEN,
        f"seed: chars must cover the requested tokens: {seeded.seed}",
    )
    _check(
        shapes.SEED_MODEL in shapes.model_ids(seeded),
        "seed: the seed model must be declared alongside the shape models",
    )
    first_turn = seeded.seed.script.consume()[0]
    second_turn = seeded.seed.script.consume()[0]
    _check(
        first_turn is second_turn and first_turn.chunk <= shapes.SEED_FRAME_CHARS,
        "seed: the seed script must cycle (session count, not script length, bounds it)",
    )
    try:
        shapes.build_plan(_shape_args("pure_stream", history_tokens=10))
    except shapes.ShapeError as exc:
        _check("history-tokens" in str(exc), f"error must name the knob: {exc}")
    else:  # pragma: no cover - 旋钮闸门回归
        raise SelftestFailure("a shape that ignores --history-tokens must reject it")
    try:
        shapes.resolve("no-such-shape")
    except shapes.ShapeError as exc:
        _check("pure_stream" in str(exc), f"unknown shape must list the choices: {exc}")
    else:  # pragma: no cover - 注册表回归
        raise SelftestFailure("unknown scenario must raise ShapeError")


# ── verdict ───────────────────────────────────────────────────


def _level(
    sessions: int,
    *,
    delivery_p99: float | None = 2.0,
    turn_p99: float | None = None,
    cpu: float = 0.1,
    errors: int = 0,
    fds: int = 40,
    provider_cpu: float = 0.1,
    shard_cpu: Sequence[float] = (0.1,),
    count: int | None = 100,
    dropped: int | None = 0,
    dropped_reasons: Mapping[str, int] | None = None,
    create_failed: int = 0,
    error: str | None = None,
    window_mode: str = "drive",
    drives_upstream: bool = True,
    turn_boundaries: bool = False,
) -> dict[str, Any]:
    record = level_skeleton(sessions)
    record["shape"].update(
        {
            "window_mode": window_mode,
            "drives_upstream": drives_upstream,
            "turn_boundaries": turn_boundaries,
        }
    )
    if error is None:
        record["wall_s"] = 5.0
        record["metrics"].update(
            {
                "gateway.cpu_cores": cpu,
                "gateway.fds_peak": fds,
                "delivery.p99_ms": delivery_p99,
                "turn.p99_ms": turn_p99,
                "errors": errors,
            }
        )
        record["rig"].update(
            {"provider_cpu_cores": provider_cpu, "shard_cpu_cores": list(shard_cpu)}
        )
        record["samples"].update(
            {
                "count": count,
                "dropped": dropped,
                "dropped_reasons": None
                if dropped_reasons is None
                else dict(dropped_reasons),
            }
        )
        record["session_create"].update(
            {"ok": sessions, "failed": create_failed, "total_ms": 1.0}
        )
    else:
        record["error"] = error
    return record


def _run(*levels: dict[str, Any], scenario: str = "pure_stream") -> dict[str, Any]:
    return {
        "meta": {
            "tool": "capacity",
            "schema": 1,
            "scenario": scenario,
            "started_at": "2026-01-01T00:00:00Z",
            "duration_s": 10.0,
            "args": {
                "levels": [record["sessions"] for record in levels],
                "rate": 50.0,
                "chunk": 1,
                "dwell": 5.0,
                "warmup": 1.0,
                "shards": 1,
                "subscribers": "per-session",
                "backend": "file",
                "tool": "read",
                "cycle_s": 3.0,
                "decode_duty": 0.7,
                "history_tokens": 0,
                "provider_max_connections": None,
            },
            "host": {"cpus": 12, "mem_gb": 16.0, "platform": "test", "ulimit_n": 4096},
            "gateway": {"pid": 1, "version": "test", "commit": "abcdef0"},
            "session_create": {"ok": 1, "failed": 0, "total_ms": 1.0},
            "notes": [],
            "failures": [
                f"level {r['sessions']}: {r['error']}" for r in levels if r.get("error")
            ],
        },
        "levels": list(levels),
    }


def check_verdict() -> None:
    """判据：baseline / degraded / capped / unknown / low_confidence，阈值只有一处声明。"""
    # 干净阶梯：基线最小 N，无命中。
    clean = verdict.evaluate_run(_run(_level(1), _level(10)))
    _check(
        [result.verdict for result in clean] == ["ok", "ok"],
        f"a clean ladder must be ok: {[r.verdict for r in clean]}",
    )
    _check(
        all(result.low_confidence is False for result in clean),
        "a clean ladder must not be low confidence",
    )
    _check(
        all(result.baseline_sessions == 1 for result in clean),
        f"baseline must be the smallest N: {[r.baseline_sessions for r in clean]}",
    )

    # 连接池上限：SSE 长连接下 sessions > 上限 ⇒ 超出的会话排队，读数不作数。
    # 未设旗标时有效上限 = httpx 的默认值（声明在 verdict，这里交叉校验产品事实）。
    # 产品侧 ``make_http_limits(None)`` 不传 ``limits=``，落到 ``AsyncClient`` 的默认形参
    # —— 从签名里读它，httpx 改默认值这条断言就红。
    client_default = (
        inspect.signature(httpx.AsyncClient.__init__).parameters["limits"].default
    )
    _check(
        client_default.max_connections == verdict.DEFAULT_POOL_CAP,
        f"the declared default pool cap must match the httpx client default: "
        f"{verdict.DEFAULT_POOL_CAP} != {client_default.max_connections}",
    )
    default_cap = verdict.effective_pool_cap(
        {"args": {"provider_max_connections": None}}
    )
    _check(
        default_cap == verdict.DEFAULT_POOL_CAP,
        f"an unset flag must fall back to the gateway default: {default_cap}",
    )
    over_pool = verdict.evaluate_run(_run(_level(1), _level(1000)))
    _check(
        over_pool[1].low_confidence is True
        and any(
            reason.startswith(verdict.CRITERIA["low_confidence.pool"])
            for reason in over_pool[1].low_confidence_reasons
        ),
        f"1000 sessions against the default pool must be low confidence: {over_pool[1]}",
    )
    _check(
        over_pool[1].verdict == "ok",
        f"the pool cap must not turn a level into degraded: {over_pool[1]}",
    )
    wide = _run(_level(1), _level(1000))
    wide["meta"]["args"]["provider_max_connections"] = 2000
    wide_result = verdict.evaluate_run(wide)
    _check(
        wide_result[1].low_confidence is False,
        f"a pool wide enough for the level must not be flagged: {wide_result[1]}",
    )
    # 池判据按形态声明生效：同一个池上限，驱动上游的形态命中、不驱动的（``hold``）不命中。
    hold = _run(
        _level(1, window_mode="hold", drives_upstream=False),
        _level(1000, window_mode="hold", drives_upstream=False),
    )
    hold_result = verdict.evaluate_run(hold)
    _check(
        hold_result[1].low_confidence is False,
        f"a shape that holds no upstream stream cannot be pool-bound: {hold_result[1]}",
    )
    driving = verdict.evaluate_run(_run(_level(1), _level(1000)))
    _check(
        driving[1].low_confidence is True,
        "the same pool cap must still flag a shape that does hold upstream streams",
    )
    exact = verdict.evaluate_run(_run(_level(1), _level(100)))
    _check(
        exact[1].low_confidence is False,
        "sessions == cap is fine: every session still holds a connection",
    )

    # delivery 退化：> max(3 × 基线, 50 ms)。地板优先（基线 2 ms → 50 ms 才是界）。
    floor = verdict.evaluate_run(_run(_level(1), _level(10, delivery_p99=40.0)))
    _check(
        floor[1].verdict == "ok",
        f"40 ms with a 50 ms floor must not degrade: {floor[1].reasons}",
    )
    over = verdict.evaluate_run(_run(_level(1), _level(10, delivery_p99=60.0)))
    _check(over[1].verdict == "degraded", f"60 ms must degrade: {over[1].reasons}")
    _check(
        over[1].reasons and verdict.CRITERIA["degraded.delivery"] in over[1].reasons[0],
        f"reasons must quote the frozen criterion: {over[1].reasons}",
    )
    # 基线本身不与自己比。
    _check(over[0].verdict == "ok", "the baseline level cannot degrade against itself")

    # turn 判据只在声明了「窗口里有轮次边界」的形态上判（形态自己声明，不看名单）。
    single = _run(_level(1, turn_p99=100.0), _level(10, turn_p99=200.0))
    _check(
        verdict.evaluate_run(single)[1].verdict == "ok",
        "turn p99 must be ignored where the shape declares no turn boundaries",
    )
    cycling = _run(
        _level(1, turn_p99=100.0, turn_boundaries=True),
        _level(10, turn_p99=200.0, turn_boundaries=True),
    )
    _check(
        verdict.evaluate_run(cycling)[1].verdict == "degraded",
        "turn p99 must be judged where the shape declares turn boundaries",
    )

    # capped：cpu / errors / fd 任一条，优先于 degraded。
    capped_cpu = verdict.evaluate_run(_run(_level(1), _level(10, cpu=0.95)))
    _check(capped_cpu[1].verdict == "capped", f"{capped_cpu[1].reasons}")
    capped_err = verdict.evaluate_run(_run(_level(1), _level(10, errors=2)))
    _check(
        capped_err[1].verdict == "capped" and "errors" in capped_err[1].reasons[0],
        f"any turn error is capped: {capped_err[1].reasons}",
    )
    capped_fd = verdict.evaluate_run(_run(_level(1), _level(10, fds=3700)))
    _check(capped_fd[1].verdict == "capped", f"{capped_fd[1].reasons}")
    both = verdict.evaluate_run(
        _run(_level(1), _level(10, cpu=0.95, delivery_p99=500.0))
    )
    _check(
        both[1].verdict == "capped" and both[1].flags == ("degraded", "capped"),
        f"capped outranks degraded but both are flagged: {both[1]}",
    )

    # low_confidence：与 reasons 分开，且不进判档。
    low = verdict.evaluate_run(
        _run(_level(1), _level(10, provider_cpu=0.85, dropped=50, count=1000))
    )
    _check(
        low[1].verdict == "ok" and low[1].low_confidence is True,
        f"a saturated rig must not change the verdict, only the confidence: {low[1]}",
    )
    _check(
        any(
            verdict.CRITERIA["low_confidence.provider"] in reason
            for reason in low[1].low_confidence_reasons
        )
        and any(
            verdict.CRITERIA["low_confidence.drops"] in reason
            for reason in low[1].low_confidence_reasons
        ),
        f"both rig signals must be named: {low[1].low_confidence_reasons}",
    )
    _check(not low[1].reasons, f"low confidence must not leak into reasons: {low[1]}")
    # 结构性丢弃（预热段 / 窗口末在飞）不是污染：pure_stream 的每一档都有它们，
    # 按总数算会把「正常」标成「低置信」。
    structural = verdict.evaluate_run(
        _run(
            _level(1),
            _level(
                10,
                count=2500,
                dropped=1100,
                dropped_reasons={"outside_window": 1050, "in_flight_tail:content": 50},
            ),
        )
    )
    _check(
        structural[1].low_confidence is False,
        f"window bookkeeping must not be read as contamination: "
        f"{structural[1].low_confidence_reasons}",
    )
    # 不预期的丢弃（客户端少收 / 多收）才是污染信号。
    missing = verdict.evaluate_run(
        _run(
            _level(1),
            _level(
                10,
                count=1000,
                dropped=1050,
                dropped_reasons={"outside_window": 1000, "missing_events:content": 50},
            ),
        )
    )
    _check(
        missing[1].low_confidence is True,
        "a completed turn missing events is contamination",
    )
    shard_low = verdict.evaluate_run(_run(_level(1), _level(10, shard_cpu=(0.2, 0.95))))
    _check(
        shard_low[1].low_confidence is True,
        "a saturated shard must be low confidence",
    )
    # 多轮形态的轮次判据无从评估（基线档没有完成的轮次）必须点名，不能读成「没退化」。
    no_baseline_turn = verdict.evaluate_run(
        _run(
            _level(1, turn_boundaries=True),
            _level(10, turn_p99=900.0, turn_boundaries=True),
            scenario="mixed",
        )
    )
    _check(
        any(
            reason.startswith(verdict.CRITERIA["unjudged.turn"])
            for reason in no_baseline_turn[1].unjudged
        ),
        f"an unevaluable turn criterion must be named: {no_baseline_turn[1]}",
    )
    _check(
        no_baseline_turn[1].verdict == "ok"
        and no_baseline_turn[1].low_confidence is False,
        f"an unevaluable criterion is not contamination: {no_baseline_turn[1]}",
    )
    # 基线档自己有轮次时不点名。
    judged = verdict.evaluate_run(
        _run(
            _level(1, turn_p99=100.0, turn_boundaries=True),
            _level(10, turn_p99=110.0, turn_boundaries=True),
            scenario="mixed",
        )
    )
    _check(
        not judged[1].unjudged,
        f"a judgeable turn criterion must not be flagged: {judged[1]}",
    )
    create_low = verdict.evaluate_run(_run(_level(1), _level(10, create_failed=1)))
    _check(
        create_low[1].low_confidence is True,
        "failed session creates must be low confidence",
    )

    # 基线档没跑完 → unknown（不是 ok），失败的档位全 null。
    broken = verdict.evaluate_run(_run(_level(1, error="gateway died"), _level(10)))
    _check(
        broken[0].verdict is None and broken[0].low_confidence is None,
        f"a failed level has no verdict: {broken[0]}",
    )
    _check(
        broken[1].verdict == "unknown",
        f"without a baseline the degraded criteria cannot be evaluated: {broken[1]}",
    )
    _check(
        "verdict" in verdict.block(broken[1])
        and set(verdict.block(broken[1])) == set(verdict.VERDICT_KEYS),
        "the JSON block keys must match VERDICT_KEYS",
    )
    # 判据原文清单只有一处：报告直接渲染它。
    _check(
        len(verdict.CRITERIA) == 12,
        f"criteria must be one declaration: {len(verdict.CRITERIA)}",
    )
    classified = {key for keys in verdict.CRITERIA_FLAGS.values() for key in keys}
    _check(
        classified
        == {
            key
            for key in verdict.CRITERIA
            if key.split(".")[0] in ("degraded", "capped")
        },
        f"every verdict criterion must be classified for the report: {classified}",
    )
    for key in classified:
        _check(
            verdict.reason_flag(verdict.CRITERIA[key]) is not None,
            f"{key} must be recognisable from its own text",
        )
    _check(
        verdict.reason_flag("something else") is None,
        "an unknown reason must not be classified",
    )


def check_report_verdicts() -> None:
    """报告层回归：低置信 / 判据未评估 / 退化结论三者不能互相吞掉。

    这三条都是「报告说的比数据多/少」的同一族：低置信档不许被结论引用，判据未评估不许
    被读成「没问题」，而未评估也**不许**把该档真正命中的判据一起从结论里剔掉。
    """
    # 基线档没有完成的轮次（mixed 单会话落工具组之前的那种产物形态）+ 更高档投递延迟越界。
    run = _run(
        _level(1, turn_boundaries=True),
        _level(10, delivery_p99=800.0, turn_p99=1200.0, turn_boundaries=True),
        scenario="mixed",
    )
    run["meta"]["args_replayable"] = ["levels", "rate", "chunk"]
    text = report.render(run)
    _check(
        "体感退化先出现在 N=10" in text,
        "a level whose turn criterion is unjudged must still contribute its other "
        "findings to the conclusion",
    )
    _check(
        "判据未评估" in text and "干净的" not in text,
        "the unjudged banner must not claim the readings are clean",
    )
    _check(
        "照常生效" not in text,
        "the report must not promise that the remaining criteria fired",
    )
    # 同时低置信 + 未评估：两句话不得互相矛盾（一个说可能被污染，一个说干净）。
    mixed_case = _run(
        _level(1, turn_boundaries=True),
        _level(
            10,
            delivery_p99=800.0,
            turn_p99=1200.0,
            provider_cpu=0.95,
            turn_boundaries=True,
        ),
    )
    mixed_text = report.render(mixed_case)
    _check(
        "干净的" not in mixed_text,
        "a low-confidence level must never be described as clean",
    )
    _check(
        "没有引用它们" in mixed_text,
        "low-confidence levels must be excluded from the conclusion",
    )
    # 投递延迟压根没测到（窗口里没有可配对的帧）→ 这条判据必须被点名为未评估。
    blind = _run(_level(1), _level(10, delivery_p99=None))
    blind_text = report.render(blind)
    _check(
        verdict.CRITERIA["unjudged.delivery"] in blind_text,
        f"an unmeasured delivery criterion must be named: {blind_text[:400]}",
    )
    _check(
        "投递延迟未测到" in blind_text,
        "the conclusion must say the delivery axis was not measured",
    )
    # 连接池上限：横幅必须点名「哪档超出多少」，且不替这一档说「其余判据照常」。
    pooled = _run(_level(1), _level(1000, delivery_p99=800.0))
    pooled_text = report.render(pooled)
    _check(
        "连接池上限" in pooled_text and "N=1000 > 100" in pooled_text,
        f"the pool banner must name the level and the cap: {pooled_text[:400]}",
    )
    _check(
        "测的是**池**" in pooled_text,
        "the pool banner must say the window measured the pool, not the gateway",
    )
    # 池够宽（旗标 2000）时既没有横幅，也没有低置信。
    wide_run = _run(_level(1), _level(1000))
    wide_run["meta"]["args"]["provider_max_connections"] = 2000
    wide_text = report.render(wide_run)
    _check(
        "连接池上限" not in wide_text,
        "a pool wide enough for the ladder must not raise the banner",
    )
    # 复现命令按生产者声明的键回放：把声明缩到 [levels]，命令就必须只剩 --levels。
    # （这条断言钉住机制本身——旧写法靠「名单恰好覆盖两个计时键」也能过。）
    narrowed = json.loads(json.dumps(run))
    narrowed["meta"]["args_replayable"] = ["levels"]
    command = report.render(narrowed)
    line = next(
        text for text in command.splitlines() if text.startswith("uv run python")
    )
    _check(
        line.split()[4:] == ["--scenario", "mixed", "--levels", "1,10"],
        f"only the declared keys may be replayed: {line}",
    )
    # 没有声明字段（手写 / 旧版产物）时退回旧口径：计时校准值仍不许当旗标。
    undeclared = json.loads(json.dumps(run))
    undeclared["meta"].pop("args_replayable", None)
    undeclared["meta"]["args"]["frame_delay_ms"] = 17.9
    legacy = next(
        text
        for text in report.render(undeclared).splitlines()
        if text.startswith("uv run python")
    )
    _check(
        "--frame-delay-ms" not in legacy
        and "--levels 1,10" in legacy
        and "--rate" in legacy,
        f"without a declaration the legacy filter still applies: {legacy}",
    )


def check_polluted() -> None:
    """负向验证：被污染的 ``run.json`` 必须被标低置信，而不是当成有效结论。

    污染有两种现实来源：采样期间 rig 自己饱和（provider / shard CPU 超标）与
    配对大面积丢弃（dropped 比例超限）。这里把一份**手写的** ``run.json`` 落到临时
    目录再读回来渲染——报告不能因为文件里没有 ``verdict`` 字段就当作一切正常。
    """
    polluted = _run(
        _level(1),
        _level(
            10,
            delivery_p99=9.0,
            cpu=0.55,
            provider_cpu=0.92,
            shard_cpu=(0.3, 0.95),
            count=1000,
            dropped=1190,
            dropped_reasons={
                "outside_window": 1050,
                "in_flight_tail:content": 20,
                "missing_events:content": 120,
            },
        ),
    )
    with tempfile.TemporaryDirectory(prefix="capacity-selftest-") as tmp:
        path = Path(tmp) / "run.json"
        path.write_text(json.dumps(polluted), encoding="utf-8")
        loaded = json.loads(path.read_text(encoding="utf-8"))
    text = report.render(loaded)
    _check(
        "低置信" in text,
        "the report must call out low-confidence levels up front",
    )
    _check(
        "ok (low conf)" in text,
        "the polluted level's verdict cell must carry the low-confidence marker",
    )
    _check(
        "没有引用它们" in text,
        "the conclusion must say it did not cite the low-confidence levels",
    )
    _check(
        "可用档位里最大的 N=1" in text,
        "the conclusion must fall back to the largest *usable* level, not the "
        "polluted one",
    )
    _check(
        verdict.CRITERIA["low_confidence.drops"] in text,
        "the criterion that fired must be quoted verbatim",
    )
    clean_text = report.render(_run(_level(1), _level(10)))
    _check(
        "低置信" not in clean_text,
        "a clean run must not be marked low confidence",
    )


# ── report ────────────────────────────────────────────────────


def _synthetic_run() -> dict[str, Any]:
    """三档合成产物：齐备 / 延迟不可用 / 水合形态（键集相同）。"""
    first = _level(1, turn_p99=5000.0)
    first["metrics"].update(
        {
            "gateway.rss_mb_peak": 100.0,
            "gateway.rss_mb_end": 101.0,
            "delivery.p50_ms": 1.0,
            "delivery.p999_ms": 3.0,
            "tokens_per_second_actual": 50.0,
            "events_per_second_actual": 50.0,
            "chars_per_second": 50.0,
            "turns_per_second": 0.2,
            "slow_client_recycles": 0,
        }
    )
    first["samples"].update(
        {
            "dropped_reasons": {},
            "delta_frames": {"content": 250, "thinking": 0, "tool_call": 0},
            "text_frames": 250,
            "chars": 250,
            "turns": 1,
            "turns_in_flight": 0,
            "delivery_available": True,
            "sync_sessions": 0,
            "hydration_attempts": 0,
            "hydration_failed": 0,
            "evicted_by_reaper": None,
            "released_by_probe": None,
        }
    )
    first["session_create"].update({"ok": 1, "failed": 0, "total_ms": 4.0})
    second = _level(10, delivery_p99=None, cpu=1.0, errors=2)
    second["metrics"].update(
        {
            "gateway.rss_mb_peak": 200.0,
            "gateway.rss_mb_end": 202.0,
            "tokens_per_second_actual": 500.0,
            "slow_client_recycles": 1,
        }
    )
    second["rig"].update({"provider_cpu_cores": 0.5, "shard_cpu_cores": [0.4, 0.3]})
    second["rig"]["host_loadavg"] = None
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
    third = _level(100, cpu=2.5, delivery_p99=6.0)
    third["metrics"].update(
        {
            "hydration.p50_ms": 120.0,
            "hydration.p99_ms": 300.0,
            "hydration.p999_ms": 400.0,
            "first_frame.p50_ms": 70.0,
            "first_frame.p99_ms": 210.0,
        }
    )
    third["gateway"].update({"restart_ms": 640.0})
    third["shape"].update({"window_mode": "hydrate"})
    third["samples"].update({"sync_sessions": 100, "hydration_attempts": 100})
    third["history"].update(
        {"tokens_per_session": 2000, "chars_per_session": 6667, "turns_per_session": 1}
    )
    run = _run(first, second, third, scenario="hydration")
    run["meta"]["gateway"] = {"pid": 9, "version": "0.7.0", "commit": "abcdef0"}
    return run


def check_report() -> None:
    """合成 run.json → markdown：结论、判据原文、缺数据占位、复现命令、表格列数。"""
    run = _synthetic_run()
    run["levels"][2]["notes"] = ["multi\nline\ndetail"]
    text = report.render(run)
    _check(
        text.startswith("# capacity — hydration"), "report must start with its title"
    )
    _check("## 结论" in text, "the report must state a conclusion")
    _check("速度轴" in text and "内存轴" in text, "both axes must be stated")
    _check(
        "| 1 |" in text and "| 10 |" in text and "| 100 |" in text,
        "each level must get a row",
    )
    _check("| 0.10 |" in text, "gateway cpu must be rendered with two decimals")
    _check("投递延迟不可用" in text, "unavailable delivery must be called out up front")
    _check("— / —" in text, "missing metrics must render as an em dash, not 0")
    _check("nearest-rank" in text, "report must state the percentile basis")
    _check("0.40, 0.30" in text, "per-shard CPU must be listed (rig quality signal)")
    _check("capped" in text, "a capped level must be visible in the table")
    _check("## 判据" in text, "the report must print the frozen criteria")
    for criterion in verdict.CRITERIA.values():
        _check(
            criterion in text,
            f"every criterion must be rendered from verdict.CRITERIA: {criterion}",
        )
    _check(
        "uv run python scripts/capacity/run.py --scenario hydration" in text,
        "the reproduce command must round-trip the args",
    )
    _check(
        "| subscribe → sync p50 / p99 / p999 ms |" in text,
        "a hydrate run must get the hydration table",
    )
    header = separator = None
    for line in text.splitlines():
        if line.startswith("| N |") and header is None:
            header = len(line.split("|")) - 2
        elif header is not None and separator is None and line.startswith("|---"):
            separator = len(line.split("|")) - 2
    _check(
        header is not None and separator == header,
        f"table separator has {separator} cells but the header has {header} "
        "(GFM drops mismatched tables)",
    )
    _check(
        "- N=100: multi line detail" in text,
        "multi-line notes must fold onto one line inside the list item",
    )
    line = report.summary_line(run["levels"][0])
    _check("N=1:" in line and "gw_cpu=" in line, "summary line must name the level")


def check_summary() -> None:
    """``--json`` 的一行摘要：字段名是下游脚本的契约，不能悄悄改。"""
    import run as run_module  # 函数级导入：run.py 只在自测分支里依赖本模块

    payload = _synthetic_run()
    payload["meta"]["out"] = "/tmp/capacity-summary"
    run_module._fill_verdicts(payload["levels"], payload["meta"])
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
        "chars_per_second",
        "turns_per_second",
        "errors",
        "verdict",
        "low_confidence",
        "error",
    ):
        _check(key in first, f"--json level projection is missing {key!r}")
    _check(
        first["gateway_cpu_cores"]
        == payload["levels"][0]["metrics"]["gateway.cpu_cores"],
        "the projection must carry the metric value through",
    )
    _check(
        payload["levels"][0]["verdict"]["verdict"] is not None,
        "_fill_verdicts must fill the level block",
    )
    _check(
        set(payload["levels"][0]["verdict"]) == set(verdict.VERDICT_KEYS),
        "the verdict block keys must match VERDICT_KEYS",
    )
    _check(
        summary["ok"] is True and summary["failures"] == [],
        "healthy run summary (errors > 0 is not a failed level)",
    )


# ── e2e ───────────────────────────────────────────────────────


def _e2e_config(scenario: str, out: Path, **overrides: Any) -> RunConfig:
    base: dict[str, Any] = {
        "scenario": scenario,
        "levels": (E2E_SESSIONS,),
        "rate": 20.0,
        "dwell": 2.0,
        "warmup": 0.5,
        "shards": 1,
        "subscribers": "per-shard",
        "history_tokens": 0,
        "out": out,
    }
    base.update(overrides)
    return RunConfig(**base)


@dataclass(frozen=True)
class E2ECase:
    """一次 e2e：用例名 / 形态 / 该档参数（断言按形态取，见 :func:`_assert_shape_metrics`）。"""

    name: str
    scenario: str
    overrides: Mapping[str, Any] = field(default_factory=dict)


E2E_CASES: tuple[E2ECase, ...] = (
    # 这一档同时验证 `--provider-max-connections` 真的落进生成配置（见
    # `check_e2e_case`）：`ProviderConfig` 是 `extra="ignore"`，键名写错会静默消失。
    E2ECase("pure_stream", "pure_stream", {"provider_max_connections": E2E_POOL_CAP}),
    # 有节奏的轮次（``--cycle-s`` 是本工具推荐的用法）：不 pacing 时两个会话就能把一个
    # 核烧到 0.9（每轮固定成本 ~9 ms CPU × 100 轮/s），那一档本来就会被判成 capped。
    E2ECase("tool_cycle", "tool_cycle", {"rate": 10.0, "cycle_s": 0.2}),
    # 带子进程的变体：yolo 失效会让 Bash 落到「等人确认」上，轮次从此永不收口。
    E2ECase(
        "tool_cycle_bash", "tool_cycle", {"rate": 10.0, "cycle_s": 0.5, "tool": "bash"}
    ),
    # 逐出要时间：TTL = warmup + dwell/2 = 2 s（这里 warmup 0），扫描 1 s → 窗口内必然越限。
    E2ECase("idle", "idle", {"warmup": 0.0, "dwell": 4.0}),
    E2ECase("hydration", "hydration", {"dwell": 3.0, "history_tokens": 200}),
    E2ECase("mixed", "mixed", {"rate": 10.0, "cycle_s": 0.2, "decode_duty": 0.5}),
)


def check_e2e_case(run_ladder: LadderRunner, case: E2ECase) -> None:
    """迷你一档走真实路径：真网关子进程 + 真分片子进程 + 真产物 + 该形态的断言。"""
    proc.raise_nofile(proc.nofile_target(E2E_SESSIONS))
    with tempfile.TemporaryDirectory(prefix=f"capacity-selftest-{case.name}-") as tmp:
        out = Path(tmp) / "run"
        cfg = _e2e_config(case.scenario, out, **case.overrides)
        started = time.monotonic()
        payload = asyncio.run(run_ladder(cfg, selftest_run=True))
        elapsed = time.monotonic() - started
        _check(
            elapsed < E2E_BUDGET_S,
            f"{case.name} e2e took {elapsed:.1f}s, budget {E2E_BUDGET_S:.0f}s",
        )
        level = _assert_common(payload, out, case.scenario)
        _assert_shape_metrics(case.scenario, level)
        if case.scenario == "tool_cycle":
            _check(
                (level["shape"] or {}).get("tool")
                == case.overrides.get("tool", "read"),
                f"the tool variant must be recorded per level: {level['shape']}",
            )
        if case.name == "pure_stream":
            check_relative_out(run_ladder, Path(tmp))
            check_failure_shape(run_ladder, Path(tmp) / "failed", level)


def check_shard_recovery() -> None:
    """分片握手落空时：样本已在盘上就必须收回来，没写成才算失败。

    饱和档（池打开 + 长阶梯）的真实形态：窗口末仍有一大批帧堵在 socket 里，分片收尾
    可能比 60 s 的握手窗口久。为一行握手丢掉完整样本会把整档变成 ``error``——恰好是
    最需要「网关已经到顶」这个结论的时候。
    """
    payload = {
        "shard": 0,
        "window": {"start": 0.0, "end": 1.0},
        "cpu_user_s": 0.0,
        "cpu_sys_s": 0.0,
        "rss_kb": 1,
        "fds": 3,
        "turns": [],
        "events": {"content": 0},
        "chars": {"content": 0},
        "errors": 0,
        "dropped_reasons": {},
        "hydration": None,
        "sync_sessions": 0,
        "hydration_attempts": 0,
        "hydration_failed": 0,
        "slow_client_recycles": 0,
    }

    with tempfile.TemporaryDirectory(prefix="capacity-selftest-recovery-") as tmp:
        out = Path(tmp) / "shard-0.samples.json"
        handle = orchestrator._ShardHandle(
            index=0,
            process=SimpleNamespace(returncode=None),
            config_path=Path(tmp) / "shard-0.json",
            out_path=out,
            log_path=Path(tmp) / "shard-0.log",
            log_handle=None,
        )

        async def recover() -> dict[str, Any]:
            return await orchestrator._await_samples(handle)

        # 文件先落盘（分片写完样本、还没报 done）→ 必须收回来。
        out.write_text(json.dumps(payload), encoding="utf-8")
        _check(
            asyncio.run(recover())["shard"] == 0,
            "a written sample file must be recovered without the handshake",
        )

        # 半截 JSON 不是完整样本：等不到就失败，不许把残文件当读数。
        out.write_text('{"shard": 0, "turns"', encoding="utf-8")
        deadline = orchestrator.SHARD_DRAIN_TIMEOUT
        orchestrator.SHARD_DRAIN_TIMEOUT = 0.3
        try:
            try:
                asyncio.run(recover())
            except orchestrator.OrchestratorError as exc:
                _check(
                    "no usable samples" in str(exc),
                    f"a truncated sample file must not be accepted: {exc}",
                )
            else:
                raise SelftestFailure("a truncated sample file must not be accepted")
        finally:
            orchestrator.SHARD_DRAIN_TIMEOUT = deadline

        # 没有样本文件 → 分片失败（不能静默当成 0 样本）。
        out.unlink()
        orchestrator.SHARD_DRAIN_TIMEOUT = 0.3
        try:
            try:
                asyncio.run(recover())
            except orchestrator.OrchestratorError:
                pass
            else:
                raise SelftestFailure("a missing sample file must fail the shard")
        finally:
            orchestrator.SHARD_DRAIN_TIMEOUT = deadline


def check_pool_passthrough() -> None:
    """``--provider-max-connections`` 必须落进**网关读到的配置**，不传时逐字不写。

    ``ProviderConfig`` 是 ``extra="ignore"``：键名写错不会报错，只会静默用上默认池——
    那样整条阶梯又会测出「100 个会话满速 + 其余排队」的假曲线。所以这条检查读真配置。
    """
    with tempfile.TemporaryDirectory(prefix="capacity-selftest-pool-") as tmp:
        root = Path(tmp)

        def rendered(**overrides: Any) -> str:
            cfg = _e2e_config("pure_stream", root / "run", **overrides)
            plan = shapes.build_plan(orchestrator.level_args(cfg, E2E_SESSIONS))

            async def run() -> str:
                env = await orchestrator._start_env(cfg, root / "env", plan)
                try:
                    return env.config_path.read_text(encoding="utf-8")
                finally:
                    await env.stop()

            return asyncio.run(run())

        written = rendered(provider_max_connections=E2E_POOL_CAP)
        _check(
            f"max_connections: {E2E_POOL_CAP}" in written,
            "an explicit --provider-max-connections must reach the config the gateway "
            f"reads; providers block: {_provider_block(written)}",
        )
        default = rendered()
        _check(
            "max_connections" not in default,
            "without the flag the generated config must not gain the key (the default "
            f"run stays byte-identical): {_provider_block(default)}",
        )


def _provider_block(config_text: str) -> str:
    """配置里 provider 段的前几行（失败文案用）。"""
    lines = config_text.splitlines()
    start = next(
        (index for index, line in enumerate(lines) if line.startswith("providers:")), 0
    )
    return " / ".join(line.strip() for line in lines[start : start + 8])


def _assert_common(
    payload: Mapping[str, Any], out: Path, scenario: str
) -> Mapping[str, Any]:
    """每个形态都一样的那部分：身份、键集、产物形状、会话数、rig 采样。"""
    meta = payload["meta"]
    _check(meta["tool"] == "capacity" and meta["schema"] == 1, "meta identity")
    _check(meta.get("selftest") is True, "meta.selftest must mark a self-check run")
    _check(int(meta["host"]["ulimit_n"]) > 0, "meta.host.ulimit_n must be recorded")
    _check(meta["failures"] == [], f"{scenario} run must not fail: {meta['failures']}")
    for key in ("chunk", "backend", "tool", "cycle_s", "decode_duty"):
        _check(key in meta["args"], f"meta.args must record {key} for an A/B rerun")
    levels = payload["levels"]
    _check(len(levels) == 1, "one level requested, one level reported")
    level = levels[0]
    _check(level.get("error") is None, f"level failed: {level.get('error')}")
    _check(level["sessions"] == E2E_SESSIONS, "level must report the requested N")
    _check(
        level["session_create"]["ok"] == E2E_SESSIONS, "all sessions must be created"
    )
    _check(
        level["verdict"]["verdict"] == "ok",
        f"a healthy single level must be judged ok: {level['verdict']}",
    )
    metrics_block = level["metrics"]
    for key in orchestrator.METRIC_KEYS:
        _check(key in metrics_block, f"missing frozen metric key {key!r}")
    for key in orchestrator.SAMPLE_KEYS:
        _check(key in level["samples"], f"missing sample key {key!r}")
    for key in orchestrator.HISTORY_KEYS:
        _check(key in level["history"], f"missing history key {key!r}")
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
        metrics_block["slow_client_recycles"] == 0,
        "a healthy window must not count the driver's own teardown as a recycle: "
        f"{metrics_block['slow_client_recycles']}",
    )
    _check(level["rig"]["shard_cpu_cores"], "shard CPU must be reported (rig signal)")
    unknown_reasons = [
        reason
        for reason in (level["samples"].get("dropped_reasons") or {})
        if metrics.drop_reason_family(reason) is None
    ]
    _check(
        not unknown_reasons,
        f"every dropped-frame reason must be in metrics.DROP_REASONS "
        f"(otherwise the verdict cannot tell bookkeeping from contamination): "
        f"{unknown_reasons}",
    )
    span_note = [note for note in level["notes"] if "CPU window" in note]
    _check(not span_note, f"shard CPU window must match the sample window: {span_note}")
    run_json = out / "run.json"
    report_md = out / "report.md"
    shard_file = out / "raw" / str(E2E_SESSIONS) / "shard-0.samples.json"
    _check(run_json.is_file(), f"run.json must exist ({run_json})")
    _check(report_md.is_file(), f"report.md must exist ({report_md})")
    _check(shard_file.is_file(), f"raw shard samples must exist ({shard_file})")
    on_disk = json.loads(run_json.read_text(encoding="utf-8"))
    _check(
        on_disk["levels"][0]["sessions"] == E2E_SESSIONS,
        "run.json must round-trip the level",
    )
    _check(
        f"# capacity — {scenario}" in report_md.read_text(encoding="utf-8"),
        "report.md must be rendered markdown",
    )
    _check(
        not (out / "raw" / str(E2E_SESSIONS) / "workspace").exists(),
        "a completed level must not leave its scratch workspace behind",
    )
    return level


def _assert_shape_metrics(scenario: str, level: Mapping[str, Any]) -> None:
    """每种形态自己的判据（结构性的 null 与真实的读数必须分得清）。"""
    block = level["metrics"]
    samples = level["samples"]
    if scenario == "pure_stream":
        _check(
            samples["turns"] == 0 and samples["turns_in_flight"] >= 1,
            f"pure_stream is single-turn: {samples['turns']} completed, "
            f"{samples['turns_in_flight']} in flight",
        )
        _check(
            block["turn.p50_ms"] is None,
            "no completed turn must leave turn.* null (never 0)",
        )
        _check(
            any(
                "no turn completed inside the window" in note for note in level["notes"]
            ),
            f"the single-turn shape must explain the null turn metric: {level['notes']}",
        )
        _check(
            samples["delivery_available"] is True,
            f"the emitter clock is part of the tree: {samples['delivery_available']}",
        )
        _check(
            block["delivery.p50_ms"] is not None
            and 0.0 <= block["delivery.p50_ms"] <= block["delivery.p99_ms"],
            f"available delivery must have ordered values: {block['delivery.p99_ms']}",
        )
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
        _check(not unexpected, f"channel pairing must close: {unexpected}")
        _check(
            samples["chars"] >= samples["delta_frames"]["content"],
            f"chars must be measured per event (>= one char each): {samples['chars']}",
        )
    elif scenario == "tool_cycle":
        _check(
            samples["turns"] > 0,
            f"tool_cycle must complete turns inside the window: {samples['turns']}",
        )
        _check(
            block["turn.p50_ms"] is not None and block["turn.p99_ms"] is not None,
            f"completed turns must yield turn latencies: {block['turn.p50_ms']}",
        )
        _check(
            block["turns_per_second"] > 0,
            f"turns/s is the per-turn fixed cost divisor: {block['turns_per_second']}",
        )
        _check(
            samples["delta_frames"]["tool_call"] > 0,
            f"the tool call must stream args: {samples['delta_frames']}",
        )
        _check(block["errors"] == 0, f"no turn may fail: {block['errors']}")
    elif scenario == "idle":
        _check(
            samples["delta_frames"]["content"] == 0 and samples["chars"] == 0,
            f"idle drives nothing: {samples['delta_frames']}",
        )
        _check(
            block["events_per_second_actual"] is None
            and block["chars_per_second"] is None,
            f"no events means null, not 0: {block['events_per_second_actual']} / "
            f"{block['chars_per_second']}",
        )
        _check(
            any("no delta event arrived" in note for note in level["notes"]),
            f"the null rate must be explained: {level['notes']}",
        )
        evicted = samples["evicted_by_reaper"]
        loaded = samples["released_by_probe"]
        _check(
            isinstance(evicted, int) and isinstance(loaded, int),
            f"the eviction probe must report counts: {evicted} / {loaded}",
        )
        _check(
            evicted + loaded == E2E_SESSIONS,
            f"every session must be accounted for: {evicted} + {loaded}",
        )
        _check(
            evicted == E2E_SESSIONS,
            f"the reaper must have evicted the idle sessions inside the window: "
            f"{evicted} of {E2E_SESSIONS}",
        )
        _check(
            (level["gateway"] or {}).get("restart_ms") is None,
            "idle must not restart the gateway",
        )
    elif scenario == "hydration":
        _check(
            (level["gateway"] or {}).get("restart_ms") is not None,
            "hydration must restart the gateway",
        )
        _check(
            level["shape"]["window_mode"] == "hydrate",
            f"hydration must report its window mode: {level['shape']}",
        )
        _check(
            samples["hydration_attempts"] == E2E_SESSIONS
            and samples["hydration_failed"] == 0,
            f"every subscribe must succeed: {samples['hydration_attempts']} / "
            f"{samples['hydration_failed']}",
        )
        _check(
            samples["sync_sessions"] >= E2E_SESSIONS,
            f"every session must hydrate inside the window: {samples['sync_sessions']}",
        )
        _check(
            block["hydration.p50_ms"] is not None
            and block["hydration.p99_ms"] is not None,
            f"hydration latency must be measured: {block['hydration.p50_ms']}",
        )
        _check(
            block["first_frame.p50_ms"] is not None,
            f"first frame after hydration must be measured: {block['first_frame.p50_ms']}",
        )
        _check(
            samples["turns"] >= 1,
            f"hydration drives one turn per session: {samples['turns']}",
        )
        _check(
            level["history"]["tokens_per_session"] == 200
            and level["history"]["total_ms"] is not None,
            f"seeded history must be recorded: {level['history']}",
        )
    elif scenario == "mixed":
        _check(
            samples["delta_frames"]["content"] > 0
            and samples["delta_frames"]["tool_call"] > 0,
            f"mixed must show both groups: {samples['delta_frames']}",
        )
        _check(
            samples["turns"] >= 1,
            f"the cycling group must finish turns: {samples['turns']}",
        )
    else:  # pragma: no cover - 注册表与用例表的一致性
        raise SelftestFailure(f"no assertions for scenario {scenario!r}")


def check_relative_out(run_ladder: LadderRunner, tmp: Path) -> None:
    """``--out`` 相对路径（也是默认形态）必须能跑完：分片以档位目录为 cwd 起。"""
    previous = Path.cwd()
    os.chdir(tmp)
    try:
        cfg = _e2e_config("pure_stream", Path("relative-run"))
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
        payload = asyncio.run(
            run_ladder(_e2e_config("pure_stream", out), selftest_run=True)
        )
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
    for block in (
        "metrics",
        "samples",
        "history",
        "gateway",
        "rig",
        "session_create",
        "shape",
        "verdict",
    ):
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
        bad["verdict"]["verdict"] is None,
        "a failed level must not get a verdict (not even ok)",
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
        ("cli.contract", check_cli),
        ("shapes.contract", check_shapes),
        ("verdict.criteria", check_verdict),
        ("verdict.polluted", check_polluted),
        ("report.verdicts", check_report_verdicts),
        ("pool.passthrough", check_pool_passthrough),
        ("shard.recovery", check_shard_recovery),
        ("report.render", check_report),
        ("run.summary", check_summary),
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
    for case in E2E_CASES:
        name = f"e2e.{case.name}"
        try:
            check_e2e_case(run_ladder, case)
        except Exception as exc:  # noqa: BLE001 - 同上
            failures.append(f"{name}: {type(exc).__name__}: {exc}")
            print(f"[selftest] FAIL {name}: {exc}", file=sys.stderr)
            continue
        passed += 1
        print(f"[selftest] ok   {name}")
    elapsed = time.monotonic() - started
    print(f"[selftest] {passed} passed · {len(failures)} failed · {elapsed:.1f}s")
    if failures:
        for failure in failures:
            print(f"[selftest] failure: {failure}", file=sys.stderr)
        return 1
    return 0
