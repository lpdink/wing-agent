#!/usr/bin/env python3
"""投递延迟配对 + 百分位统计（本工具唯一的数学处）。

两条口径必须写在数据旁边，因为它们是「数字可信」的全部：

1. **百分位是 nearest-rank**（p99 = 升序第 ``ceil(0.99·n)`` 个值，不做线性插值），
   与 ``scripts/perf/suite_gateway.py`` 一致——两处不共享实现（那边住在一个套件
   脚本里，跨工具 import 会把整条 perf 测量链拉进来），但秩定义相同。
2. **投递延迟按通道、按下标配对**：假 Provider 的每帧带一个通道标记
   （``FrameEmit.kind``），可配对的三个通道各有自己的客户端事件（见
   :data:`CHANNEL_EVENTS`）：``content`` ↔ ``text``、``thinking`` ↔ ``reasoning``、
   ``tool_call`` ↔ ``tool_call_stream``；``meta``（role / finish / usage / ``[DONE]``
   帧）不产生 delta 事件，**不参与配对**。同一通道内再按下标一一对齐（第 i 帧 ↔ 第 i
   个事件）。配不上的帧一律记 ``dropped`` 并按原因分类，
   **绝不截断对齐、绝不插值、绝不把配不上的样本当 0**。丢弃计数与原因分类的**单位
   都是帧**（``sum(reasons.values()) == dropped``）。

   同一个 prompt 文本命中多个请求时按**通道**消歧：工具轮的两段式（调用轮 + 文本轮）
   与网关重试都会让一个 prompt 对应两次请求，而某个通道的帧只可能来自发过该通道帧的
   那一次请求。仍有多条候选（真重试）才整轮丢弃——不猜。

发射时刻来自上游冻结接口（``FakeProvider.frame_emits``）；缺失时
:func:`build_emit_ledger` 直接抛 :class:`DeliveryUnavailable`，由档位编排
（``orchestrator``）转成「延迟指标不可用」并三处点名（不写 0、不静默）。
"""

from __future__ import annotations

import math
from collections import Counter
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

#: 通道标记 → 客户端 delta 事件类型（``sse.FRAME_KINDS`` 里可配对的三类）。
#:
#: 这一张表是配对口径的唯一声明：分片按它把到达事件归档，配对按它把发射帧归档。
#: ``meta``（role / finish / usage / ``[DONE]``）**不产生** delta 事件，故不在此表。
CHANNEL_EVENTS: dict[str, str] = {
    "content": "text",
    "thinking": "reasoning",
    "tool_call": "tool_call_stream",
}

#: 客户端事件类型 → 通道标记（分片侧的归档方向）。
EVENT_CHANNELS: dict[str, str] = {event: kind for kind, event in CHANNEL_EVENTS.items()}

#: 配对丢弃的原因词汇表（**唯一声明处**）：``reason → 是否窗口簿记（不是数据被污染）``。
#:
#: 带 ``:<kind>`` 后缀的原因由前缀匹配（见 :func:`drop_reason_family`）。这张表同时被
#: 生产者（:func:`pair_delivery`）与消费者（``verdict`` 判低置信丢弃率）使用——两处各写
#: 一份字面量的话，改一个名字只会让真实运行里每一档突然「低置信」，编译器与本目录自测
#: 都不会响。
DROP_REASONS: dict[str, str] = {
    "outside_window": "bookkeeping",
    "in_flight_tail": "bookkeeping",
    "missing_events": "contamination",
    "unexpected_events": "contamination",
    "negative_latency": "contamination",
    "unknown_prompt": "contamination",
    "ambiguous_prompt": "contamination",
}

#: 判据侧关心的那一类：簿记原因（跨窗口起点的预热段 / 窗口末仍在传输）。
BOOKKEEPING_DROP_REASONS: tuple[str, ...] = tuple(
    reason for reason, kind in DROP_REASONS.items() if kind == "bookkeeping"
)


def drop_reason_family(reason: str) -> str | None:
    """原因名（可带 ``:<kind>`` 后缀）→ 词汇表里的名字；不认识的返回 ``None``。

    不认识的原因名一律按污染处理（保守），并在报告里原样出现——它就是诊断信息。
    """
    base = str(reason).split(":", 1)[0]
    return base if base in DROP_REASONS else None


#: 报告呈现的三个分位（label, 百分位）。
SPREAD_PERCENTILES: tuple[tuple[str, float], ...] = (
    ("p50", 50.0),
    ("p99", 99.0),
    ("p999", 99.9),
)


class DeliveryUnavailable(RuntimeError):
    """假 Provider 不提供逐帧发射时刻——投递延迟不可测。"""


#: 分片样本文件（跨进程契约）的必填键。两侧都过 :func:`check_shard_payload`：
#: 协调者按名取值，缺键在那边会静默变成 0——与「缺数据绝不写 0」正好相反。
SHARD_PAYLOAD_KEYS: frozenset[str] = frozenset(
    {
        "shard",
        "gateway_url",
        "subscribers",
        "shape",
        "ws_connections",
        "sessions",
        "frames_received",
        "cpu_seconds",
        "cpu_span_s",
        "closed_before_end",
        "delta_frames",
        "text_frames",
        "chars",
        "sync_sessions",
        "hydration_attempts",
        "hydration_failed",
        "errors",
        "error_details",
        "window",
        "turns",
        "turns_in_flight",
    }
)


def check_shard_payload(payload: Mapping[str, Any]) -> None:
    """校验分片样本的必填键（缺键即抛，不做 ``get(key, 0)`` 这类兜底）。"""
    missing = SHARD_PAYLOAD_KEYS - set(payload)
    if missing:
        raise ValueError(f"shard sample payload is missing keys: {sorted(missing)}")


def percentile(values: Sequence[float], pct: float) -> float:
    """nearest-rank 百分位（``pct`` ∈ (0, 100]）：升序第 ``ceil(pct/100·n)`` 个值。

    空序列抛 ``ValueError``——「没有样本」与「样本为 0」是两件事，调用方据此写
    ``null`` 而不是 0。
    """
    if not values:
        raise ValueError("percentile() of an empty sequence")
    ordered = sorted(values)
    rank = max(1, math.ceil(pct / 100.0 * len(ordered)))
    return float(ordered[min(rank, len(ordered)) - 1])


def spread(values: Sequence[float]) -> dict[str, float]:
    """``{"p50": …, "p99": …, "p999": …}``；空序列 → ``{}``（调用方写 null）。"""
    if not values:
        return {}
    return {label: percentile(values, pct) for label, pct in SPREAD_PERCENTILES}


# ── 投递延迟配对 ───────────────────────────────────────────────


@dataclass(frozen=True)
class TurnSample:
    """一轮的客户端侧事实（绝对 ``time.monotonic()`` 秒）。

    ``prompt`` 是驱动方发出的**全局唯一**文本（配对键）；``sent_at`` 是发出请求的
    时刻；``result_at`` 是 ``turn_result`` 到达的时刻（``None`` = 窗口结束时这轮仍在飞，
    此时到达时刻只到窗口末为止）；``arrivals`` 是**按通道**的到达时刻（通道标记 →
    按到达序的时刻），不含 ``meta`` 帧对应的东西（它们没有客户端事件）。

    ``hydrate_sent_at`` / ``hydrated_at`` 只在水合形态上有值：订阅请求发出的时刻与
    ``sync_session`` 到达的时刻（两段之间的差就是「水合」——与「首帧」是两条不同的
    时间线，见 ``orchestrator._fill_hydration``）。
    """

    prompt: str
    sent_at: float
    result_at: float | None
    arrivals: Mapping[str, Sequence[float]]
    hydrate_sent_at: float | None = None
    hydrated_at: float | None = None


@dataclass
class Pairing:
    """配对结果：可用延迟样本 + 丢弃计数（按原因、按通道）。"""

    latencies_ms: list[float] = field(default_factory=list)
    dropped: int = 0
    reasons: Counter[str] = field(default_factory=Counter)
    channels: Counter[str] = field(default_factory=Counter)

    @property
    def paired(self) -> int:
        return len(self.latencies_ms)


@dataclass(frozen=True)
class EmitLedger:
    """发射时刻账本：``request_index → {kind → 按 frame_index 升序的时刻}``。

    构造一次、配对时 O(1) 查。``meta`` 帧在构造时就被丢弃（它们没有客户端事件），
    所以账本里每一条都与某个客户端 delta 事件可配对。
    """

    by_request: Mapping[int, Mapping[str, tuple[float, ...]]]

    def kinds(self, request_index: int) -> Mapping[str, tuple[float, ...]]:
        return self.by_request.get(request_index, {})


def build_emit_ledger(provider: Any) -> EmitLedger:
    """从 ``FakeProvider.frame_emits`` 建按通道分组的账本（缺接口即抛）。"""
    emits = getattr(provider, "frame_emits", None)
    if emits is None:
        raise DeliveryUnavailable(
            "frame_emits not available: the fake provider does not record per-frame "
            "emit times, so delivery latency cannot be paired against the emitter "
            "clock (needs FakeProvider.frame_emits)"
        )
    grouped: dict[int, dict[str, list[tuple[int, float]]]] = {}
    for entry in emits:
        kind = getattr(entry, "kind", None)
        if kind not in CHANNEL_EVENTS:
            continue  # meta（role / finish / usage / [DONE]）不产生客户端 delta 事件
        grouped.setdefault(int(entry.request_index), {}).setdefault(kind, []).append(
            (int(entry.frame_index), float(entry.at))
        )
    return EmitLedger(
        {
            request_index: {
                kind: tuple(at for _, at in sorted(pairs))
                for kind, pairs in kinds.items()
            }
            for request_index, kinds in grouped.items()
        }
    )


def content_text(content: Any) -> str | None:
    """消息 content → 纯文本（``str`` 形态直接返回，block 列表拼 ``text`` 块）。"""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = [
            block["text"]
            for block in content
            if isinstance(block, Mapping)
            and block.get("type") == "text"
            and isinstance(block.get("text"), str)
        ]
        if parts:
            return "".join(parts)
    return None


def last_user_text(body: Mapping[str, Any]) -> str | None:
    """请求体里**最后一条 user 消息**的文本（配对键的来源）。

    网关把用户正文序列化成 block 列表（带 ``cache_control``），所以不能只认 ``str``。
    """
    messages = body.get("messages")
    if not isinstance(messages, list):
        return None
    for message in reversed(messages):
        if not isinstance(message, Mapping) or message.get("role") != "user":
            continue
        return content_text(message.get("content"))
    return None


def prompt_index(requests: Iterable[Any]) -> dict[str, list[int]]:
    """``prompt 文本 → 命中的 request_index 列表``（重试会让同一文本出现多次）。"""
    index: dict[str, list[int]] = {}
    for entry in requests:
        body = getattr(entry, "body", None)
        if not isinstance(body, Mapping):
            continue
        text = last_user_text(body)
        if text is None:
            continue
        index.setdefault(text, []).append(int(entry.index))
    return index


def _arrival_count(turn: TurnSample) -> int:
    return sum(len(values) for values in turn.arrivals.values())


def pair_delivery(
    ledger: EmitLedger,
    requests: Iterable[Any],
    turns: Sequence[TurnSample],
    *,
    window: tuple[float, float] | None = None,
) -> Pairing:
    """把客户端到达时刻与 Provider 发射时刻**按通道、按下标**配对成延迟样本（毫秒）。

    同一通道内第 i 帧 ↔ 第 i 个事件；通道之间互不相干（所以 ``meta`` 帧不会让对齐
    失败）。段内对齐取两者较短的长度（前缀配对），余下的按方向记账：发射多于到达
    是「客户端还没收到」，到达多于发射是「客户端多收了」——**不猜、不插值**。

    一个 prompt 命中多个请求时（工具轮的两段式：调用轮发 ``tool_call`` 帧、文本轮发
    ``content`` 帧；或网关重试）先按通道消歧：**该通道的帧只可能来自发过该通道帧的
    那次请求**。仍有多个候选才算 ``ambiguous_prompt``（真重试，无法归因）。

    给了 ``window`` 时，落点不在窗口内的配对（跨窗口起点的那些）不计入延迟分布，
    单独记 ``outside_window``。

    原因名一律取自 :data:`DROP_REASONS`（唯一声明处）——判据侧按同一张表判断哪类
    丢弃是污染、哪类只是窗口簿记。
    """
    result = Pairing()
    index = prompt_index(requests)
    for turn in turns:
        matches = index.get(turn.prompt, [])
        if not matches:
            result.reasons["unknown_prompt"] += _arrival_count(turn)
            result.dropped += _arrival_count(turn)
            continue
        kinds = set(turn.arrivals)
        for request_index in matches:
            kinds.update(ledger.kinds(request_index))
        for kind in sorted(kinds):
            arrivals = turn.arrivals.get(kind, ())
            candidates = [
                request_index
                for request_index in matches
                if kind in ledger.kinds(request_index)
            ]
            if not candidates:
                result.reasons[f"unexpected_events:{kind}"] += len(arrivals)
                result.dropped += len(arrivals)
                continue
            if len(candidates) > 1:
                # 同一 prompt 触发多次携带该通道的请求（网关重试）——哪一次对应这批
                # 到达无法确定。发射侧富余同样无法归因，不计数。
                result.reasons["ambiguous_prompt"] += len(arrivals)
                result.dropped += len(arrivals)
                continue
            emits = ledger.kinds(candidates[0]).get(kind, ())
            paired = min(len(emits), len(arrivals))
            for position in range(paired):
                delta_ms = (arrivals[position] - emits[position]) * 1000.0
                if delta_ms < 0.0:
                    # 到达早于发射：同机单调时钟上只可能是错位（帧序对不上）。
                    result.reasons["negative_latency"] += 1
                    result.dropped += 1
                    continue
                if window is not None and not (
                    window[0] <= arrivals[position] <= window[1]
                ):
                    result.reasons["outside_window"] += 1
                    result.dropped += 1
                    continue
                result.latencies_ms.append(delta_ms)
                result.channels[kind] += 1
            if len(emits) > paired:
                # 窗口结束时仍在飞的那一轮：Provider 已经写出的帧还没到客户端。
                reason = (
                    f"missing_events:{kind}"
                    if turn.result_at is not None
                    else f"in_flight_tail:{kind}"
                )
                result.reasons[reason] += len(emits) - paired
                result.dropped += len(emits) - paired
            if len(arrivals) > paired:
                result.reasons[f"unexpected_events:{kind}"] += len(arrivals) - paired
                result.dropped += len(arrivals) - paired
    return result


def has_frame_emits(provider: Any) -> bool:
    """上游冻结接口是否已就位（档位开始前的一次能力探测）。"""
    return getattr(provider, "frame_emits", None) is not None
