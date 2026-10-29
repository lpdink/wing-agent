#!/usr/bin/env python3
"""判据：什么算退化、什么算封顶、什么读数不可信——**本工具唯一的阈值声明处**。

判档是「看图说话」的反面：每个结论都要指回一条写死的判据原文。因此阈值是常量、
判据原文是 :data:`CRITERIA` 里的字符串，报告从这张表渲染（不在文案里另写一份数字）。

    baseline = 阶梯里最小 N 那一档的同名指标

- ``degraded``（体感退化）：``delivery.p99_ms > max(3 × baseline delivery p99, 50 ms)``
  或 ``turn.p99_ms > 1.2 × baseline turn p99``（后者只在多轮形态判：单轮形态的窗口里
  没有轮次边界，``turn.*`` 结构性为 ``null``）；
- ``capped``（撞到天花板）：``gateway.cpu_cores >= 0.9`` 或 ``errors > 0`` 或
  ``gateway.fds_peak >= 0.9 × meta.host.ulimit_n``；
- ``low_confidence``（**不进判档**，但报告必须点名）：``rig.provider_cpu_cores >= 0.8``、
  任一 ``rig.shard_cpu_cores >= 0.9``、``samples.dropped / samples.count > 1%``、
  ``session_create.failed > 0``——这四条都意味着「这一档的读数可能是 rig 自己造成的」。

``verdict`` 取值 ``ok`` / ``degraded`` / ``capped`` / ``unknown``：``capped`` 优先于
``degraded``（同一档两者都命中时，天花板是更强的结论）；``unknown`` 不是第五个阈值，
而是「基线档没跑完、退化判据无从评估」——与「缺数据一律 ``null``」同一条纪律，
它保证「跑挂了」不会被读成「跑得很好」。
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

# ── 阈值（唯一声明处：报告文案也不许另写一份）──────────────────

DEGRADED_DELIVERY_FACTOR = 3.0
DEGRADED_DELIVERY_FLOOR_MS = 50.0
DEGRADED_TURN_FACTOR = 1.2

CAPPED_CPU_CORES = 0.9
CAPPED_FDS_FRACTION = 0.9

LOW_CONF_PROVIDER_CORES = 0.8
LOW_CONF_SHARD_CORES = 0.9
LOW_CONF_DROPPED_RATIO = 0.01

#: 只有「每轮都会收口」的形态才评 ``turn.p99``（其余形态的窗口里没有轮次边界）。
TURN_SCENARIOS: frozenset[str] = frozenset({"tool_cycle", "mixed"})

#: **结构性**丢弃（不是数据被污染）：跨窗口起点的预热段、窗口末仍在传输的帧。
#: ``samples.dropped`` 包含它们，而 ``pure_stream`` 的整轮本来就从 warmup 起跑——
#: 拿总数当污染信号会让每个档都「低置信」，那是把口径当成了噪声。
STRUCTURAL_DROP_REASONS: tuple[str, ...] = ("outside_window", "in_flight_tail:")

#: 判据原文（报告与 ``reasons[]`` 都从这里取，不许就地拼阈值）。
CRITERIA: dict[str, str] = {
    "degraded.delivery": (
        f"delivery.p99_ms > max({DEGRADED_DELIVERY_FACTOR:g} × baseline delivery p99, "
        f"{DEGRADED_DELIVERY_FLOOR_MS:g} ms)"
    ),
    "degraded.turn": (
        f"turn.p99_ms > {DEGRADED_TURN_FACTOR:g} × baseline turn p99 "
        "(tool_cycle / mixed only)"
    ),
    "capped.cpu": f"gateway.cpu_cores >= {CAPPED_CPU_CORES:g}",
    "capped.errors": "errors > 0",
    "capped.fds": f"gateway.fds_peak >= {CAPPED_FDS_FRACTION:g} × meta.host.ulimit_n",
    "low_confidence.provider": (
        f"rig.provider_cpu_cores >= {LOW_CONF_PROVIDER_CORES:g}"
    ),
    "low_confidence.shards": f"any rig.shard_cpu_cores >= {LOW_CONF_SHARD_CORES:g}",
    "low_confidence.drops": (
        f"unexpected dropped frames / samples.count > {LOW_CONF_DROPPED_RATIO:.0%} "
        "(outside_window / in_flight_tail excluded: window bookkeeping, not "
        "contamination)"
    ),
    "low_confidence.create": "session_create.failed > 0",
}

#: 判档的优先序（同一档命中多条时，报告与该字段都按这个顺序排）。
FLAG_ORDER: tuple[str, ...] = ("degraded", "capped")

#: 判据 → 它属于哪一档（报告按类取「第一条命中的原因」，不靠猜字符串）。
CRITERIA_FLAGS: dict[str, tuple[str, ...]] = {
    "degraded": ("degraded.delivery", "degraded.turn"),
    "capped": ("capped.cpu", "capped.errors", "capped.fds"),
}

#: 逐档结果的键（``run.json`` 的 ``levels[i].verdict`` 与报告共用）。
VERDICT_KEYS: tuple[str, ...] = (
    "verdict",
    "flags",
    "reasons",
    "low_confidence",
    "low_confidence_reasons",
    "baseline_sessions",
)


@dataclass(frozen=True)
class Verdict:
    """一档的判据结论（缺数据一律 ``None``，不是 ``ok``）。"""

    verdict: str | None
    flags: tuple[str, ...] = ()
    reasons: tuple[str, ...] = ()
    low_confidence: bool | None = None
    low_confidence_reasons: tuple[str, ...] = ()
    baseline_sessions: int | None = None


def reason_flag(reason: str) -> str | None:
    """一条判据原文属于哪一档（``degraded`` / ``capped``）；认不出即 ``None``。"""
    for flag, keys in CRITERIA_FLAGS.items():
        for key in keys:
            if reason.startswith(CRITERIA[key]):
                return flag
    return None


def first_reason(result: Verdict, flag: str) -> str | None:
    """某档第一条命中的原因（报告的一句话结论用它，避免自己拼判据文本）。"""
    return next(
        (reason for reason in result.reasons if reason_flag(reason) == flag), None
    )


def block(result: Verdict) -> dict[str, Any]:
    """``Verdict`` → ``run.json`` 的 JSON 形状（键集与 :data:`VERDICT_KEYS` 一致）。"""
    return {
        "verdict": result.verdict,
        "flags": list(result.flags),
        "reasons": list(result.reasons),
        "low_confidence": result.low_confidence,
        "low_confidence_reasons": list(result.low_confidence_reasons),
        "baseline_sessions": result.baseline_sessions,
    }


def evaluate_run(run: Mapping[str, Any]) -> list[Verdict]:
    """对 ``run.json`` 的每个档位求判据（纯函数：报告与落盘各调一次，见 README）。"""
    levels = run.get("levels") or []
    meta = run.get("meta") or {}
    scenario = meta.get("scenario")
    ulimit_n = (meta.get("host") or {}).get("ulimit_n")
    baseline = baseline_level(levels)
    return [
        evaluate_level(
            record,
            baseline=baseline,
            scenario=str(scenario) if scenario is not None else None,
            ulimit_n=ulimit_n if isinstance(ulimit_n, (int, float)) else None,
        )
        for record in levels
    ]


def baseline_level(levels: Sequence[Mapping[str, Any]]) -> Mapping[str, Any] | None:
    """基线档 = 阶梯里**最小 N** 那一档（没跑完就没有基线，退化判据不评估）。"""
    if not levels:
        return None
    smallest = min(levels, key=lambda record: int(record.get("sessions") or 0))
    return None if smallest.get("error") else smallest


def evaluate_level(
    record: Mapping[str, Any],
    *,
    baseline: Mapping[str, Any] | None,
    scenario: str | None,
    ulimit_n: float | None,
) -> Verdict:
    """单档判据（``baseline`` 为 ``None`` 时退化判据不评估 → ``unknown``）。"""
    if record.get("error"):
        # 没跑完的档位没有可判的读数：全 null，而不是「没命中判据 = ok」。
        return Verdict(verdict=None)
    reasons: list[str] = []
    flags: list[str] = []
    low: list[str] = []

    cpu = _metric(record, "gateway.cpu_cores")
    if cpu is not None and cpu >= CAPPED_CPU_CORES:
        flags.append("capped")
        reasons.append(f"{CRITERIA['capped.cpu']} — measured {cpu:g}")
    errors = _metric(record, "errors")
    if errors:
        flags.append("capped")
        reasons.append(f"{CRITERIA['capped.errors']} — measured {errors}")
    fds = _metric(record, "gateway.fds_peak")
    if fds is not None and ulimit_n:
        limit = CAPPED_FDS_FRACTION * float(ulimit_n)
        if fds >= limit:
            flags.append("capped")
            reasons.append(
                f"{CRITERIA['capped.fds']} — measured {fds} vs limit {limit:g}"
            )

    if baseline is not None:
        degraded = _degraded_reasons(record, baseline, scenario)
        if degraded:
            flags.append("degraded")
            reasons.extend(degraded)

    provider = _rig(record, "provider_cpu_cores")
    if provider is not None and provider >= LOW_CONF_PROVIDER_CORES:
        low.append(f"{CRITERIA['low_confidence.provider']} — measured {provider:g}")
    shard_cores = [value for value in _rig(record, "shard_cpu_cores") or [] if value]
    if shard_cores and max(shard_cores) >= LOW_CONF_SHARD_CORES:
        low.append(
            f"{CRITERIA['low_confidence.shards']} — measured {max(shard_cores):g}"
        )
    drop_ratio = _unexpected_drop_ratio(record)
    if drop_ratio is not None and drop_ratio > LOW_CONF_DROPPED_RATIO:
        low.append(f"{CRITERIA['low_confidence.drops']} — measured {drop_ratio:.1%}")
    create_failed = (record.get("session_create") or {}).get("failed")
    if create_failed:
        low.append(f"{CRITERIA['low_confidence.create']} — measured {create_failed}")

    ordered = tuple(flag for flag in FLAG_ORDER if flag in flags)
    if "capped" in ordered:
        verdict = "capped"
    elif "degraded" in ordered:
        verdict = "degraded"
    elif baseline is None:
        verdict = "unknown"
    else:
        verdict = "ok"
    return Verdict(
        verdict=verdict,
        flags=ordered,
        reasons=tuple(reasons),
        low_confidence=bool(low),
        low_confidence_reasons=tuple(low),
        baseline_sessions=None
        if baseline is None
        else int(baseline.get("sessions") or 0),
    )


def _degraded_reasons(
    record: Mapping[str, Any], baseline: Mapping[str, Any], scenario: str | None
) -> list[str]:
    """退化判据（逐条自带实测值，读完就知道差多远）。"""
    if int(record.get("sessions") or 0) == int(baseline.get("sessions") or 0):
        return []  # 基线档与自己比永远不退化
    reasons: list[str] = []
    p99 = _metric(record, "delivery.p99_ms")
    base_p99 = _metric(baseline, "delivery.p99_ms")
    if p99 is not None and base_p99 is not None:
        limit = max(DEGRADED_DELIVERY_FACTOR * base_p99, DEGRADED_DELIVERY_FLOOR_MS)
        if p99 > limit:
            reasons.append(
                f"{CRITERIA['degraded.delivery']} — measured {p99:g} ms vs limit "
                f"{limit:g} ms (baseline N={baseline.get('sessions')}: {base_p99:g} ms)"
            )
    if scenario in TURN_SCENARIOS:
        turn_p99 = _metric(record, "turn.p99_ms")
        base_turn = _metric(baseline, "turn.p99_ms")
        if turn_p99 is not None and base_turn is not None and base_turn > 0:
            limit = DEGRADED_TURN_FACTOR * base_turn
            if turn_p99 > limit:
                reasons.append(
                    f"{CRITERIA['degraded.turn']} — measured {turn_p99:g} ms vs limit "
                    f"{limit:g} ms (baseline N={baseline.get('sessions')}: "
                    f"{base_turn:g} ms)"
                )
    return reasons


def _metric(record: Mapping[str, Any], key: str) -> float | None:
    value = (record.get("metrics") or {}).get(key)
    return float(value) if isinstance(value, (int, float)) else None


def _rig(record: Mapping[str, Any], key: str) -> Any:
    return (record.get("rig") or {}).get(key)


def _unexpected_drop_ratio(record: Mapping[str, Any]) -> float | None:
    """**不预期**的配对丢弃率（分母 ``samples.count``）。

    ``samples.dropped`` 含 ``outside_window``（跨窗口起点的预热段）与
    ``in_flight_tail:*``（窗口末仍在传输）——两者都是采样窗口的簿记，不是数据被污染
    （``pure_stream`` 的整轮本来就跨进窗口起点，按总数算会让每一档都「低置信」）。
    只数那之外的原因：``missing_events`` / ``unexpected_events`` / ``negative_latency``
    / ``unknown_prompt`` / ``ambiguous_prompt``。``dropped_reasons`` 缺失（手写产物）时
    无法归类，按「全部不预期」处理——宁可保守地标低置信。
    """
    samples = record.get("samples") or {}
    count = samples.get("count")
    dropped = samples.get("dropped")
    if not isinstance(count, int) or not isinstance(dropped, int):
        return None
    if dropped == 0:
        return 0.0
    reasons = samples.get("dropped_reasons")
    if not isinstance(reasons, Mapping) or not reasons:
        unexpected = dropped
    else:
        unexpected = sum(
            int(value)
            for reason, value in reasons.items()
            if not str(reason).startswith(STRUCTURAL_DROP_REASONS)
        )
    if count == 0:
        return 1.0 if unexpected else 0.0
    return unexpected / count


__all__ = [
    "CRITERIA",
    "CRITERIA_FLAGS",
    "CAPPED_CPU_CORES",
    "CAPPED_FDS_FRACTION",
    "DEGRADED_DELIVERY_FACTOR",
    "DEGRADED_DELIVERY_FLOOR_MS",
    "DEGRADED_TURN_FACTOR",
    "FLAG_ORDER",
    "LOW_CONF_DROPPED_RATIO",
    "LOW_CONF_PROVIDER_CORES",
    "LOW_CONF_SHARD_CORES",
    "STRUCTURAL_DROP_REASONS",
    "TURN_SCENARIOS",
    "VERDICT_KEYS",
    "Verdict",
    "baseline_level",
    "block",
    "evaluate_level",
    "evaluate_run",
    "first_reason",
    "reason_flag",
]
