#!/usr/bin/env python3
"""投递延迟配对 + 百分位统计（本工具唯一的数学处）。

两条口径必须写在数据旁边，因为它们是「数字可信」的全部：

1. **百分位是 nearest-rank**（p99 = 升序第 ``ceil(0.99·n)`` 个值，不做线性插值），
   与 ``scripts/perf/suite_gateway.py`` 一致——两处不共享实现（那边住在一个套件
   脚本里，跨工具 import 会把整条 perf 测量链拉进来），但秩定义相同。
2. **投递延迟按帧下标严格配对**：客户端收到的第 i 个 ``text`` 事件 ↔ 假 Provider
   该请求发射的第 i 帧。长度不等、prompt 命中多条请求、负延迟 —— 一律记
   ``dropped`` 并按原因分类，**绝不截断对齐、绝不插值、绝不把配不上的样本当 0**。

发射时刻来自上游冻结接口（``FakeProvider.frame_emits`` / ``emits_for``）；缺失时
:func:`frame_emits_for` 直接抛 :class:`DeliveryUnavailable`，由档位执行器转成
「延迟指标不可用」并三处点名（不写 0、不静默）。
"""

from __future__ import annotations

import math
from collections import Counter
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

#: 报告呈现的三个分位（label, 百分位）。
SPREAD_PERCENTILES: tuple[tuple[str, float], ...] = (
    ("p50", 50.0),
    ("p99", 99.0),
    ("p999", 99.9),
)


class DeliveryUnavailable(RuntimeError):
    """假 Provider 不提供逐帧发射时刻——投递延迟不可测。"""


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
    时刻；``result_at`` 是 ``turn_result`` 到达的时刻；``arrivals`` 是窗口内收到的
    ``text`` 事件到达时刻，按到达序。
    """

    prompt: str
    sent_at: float
    result_at: float
    arrivals: tuple[float, ...]


@dataclass
class Pairing:
    """配对结果：可用延迟样本 + 丢弃计数（按原因）。"""

    latencies_ms: list[float] = field(default_factory=list)
    dropped: int = 0
    reasons: Counter[str] = field(default_factory=Counter)

    @property
    def paired(self) -> int:
        return len(self.latencies_ms)


def frame_emits_for(provider: Any, request_index: int) -> list[float]:
    """某次请求的逐帧发射时刻（``time.monotonic()`` 绝对秒，按帧序）。

    优先用 ``emits_for(request_index)``；没有该方法时退到 ``frame_emits`` 里按
    ``request_index`` 过滤、按 ``frame_index`` 排序——两者都是上游冻结接口的一部分。
    """
    emits = getattr(provider, "frame_emits", None)
    if emits is None:
        raise DeliveryUnavailable(
            "frame_emits not available: the fake provider does not record per-frame "
            "emit times, so delivery latency cannot be paired against the emitter "
            "clock (needs FakeProvider.frame_emits / emits_for)"
        )
    getter = getattr(provider, "emits_for", None)
    if callable(getter):
        return [float(value) for value in getter(request_index)]
    selected = [
        (int(entry.frame_index), float(entry.at))
        for entry in emits
        if int(entry.request_index) == request_index
    ]
    return [at for _, at in sorted(selected)]


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


def pair_delivery(
    provider: Any,
    requests: Iterable[Any],
    turns: Sequence[TurnSample],
) -> Pairing:
    """把客户端到达时刻与 Provider 发射时刻按下标配对成延迟样本（毫秒）。

    只有「prompt 恰好命中一次请求」且「发射帧数 == 到达帧数」且「延迟非负」的轮次
    才产出样本；其余整轮计入 ``dropped`` 并记原因。
    """
    result = Pairing()
    index = prompt_index(requests)
    for turn in turns:
        matches = index.get(turn.prompt, [])
        if not matches:
            result.reasons["unknown_prompt"] += 1
            result.dropped += len(turn.arrivals)
            continue
        if len(matches) > 1:
            # 同一 prompt 触发多次请求（网关重试）——哪一次对应这批到达无法确定。
            result.reasons["ambiguous_prompt"] += 1
            result.dropped += len(turn.arrivals)
            continue
        emits = frame_emits_for(provider, matches[0])
        if len(emits) != len(turn.arrivals):
            result.reasons["length_mismatch"] += 1
            result.dropped += len(turn.arrivals)
            continue
        for emit_at, arrival_at in zip(emits, turn.arrivals):
            delta_ms = (arrival_at - emit_at) * 1000.0
            if delta_ms < 0.0:
                # 到达早于发射：同机单调时钟上只可能是错位（帧序对不上）。
                result.reasons["negative_latency"] += 1
                result.dropped += 1
                continue
            result.latencies_ms.append(delta_ms)
    return result


def has_frame_emits(provider: Any) -> bool:
    """上游冻结接口是否已就位（档位开始前的一次能力探测）。"""
    return getattr(provider, "frame_emits", None) is not None


__all__ = [
    "DeliveryUnavailable",
    "Pairing",
    "SPREAD_PERCENTILES",
    "TurnSample",
    "content_text",
    "frame_emits_for",
    "has_frame_emits",
    "last_user_text",
    "pair_delivery",
    "percentile",
    "prompt_index",
    "spread",
]
