"""帧切分的重试轮：每轮必须重新装帧（信封预留变了，上一轮的帧作废）。

`build_frames` 的帧数是**先估后校**的：先用 `count_hint`（按原始字节估算）预留
信封宽度，切完拿到真实帧数就重算预留、必要时重切一轮。转义膨胀（引号 / 反斜杠
各 +1 字节）会让真实帧数远大于这个估算——位数跨过 10 / 100 时预留变宽，于是必然
进入第二轮。

一旦第二轮把上一轮的帧继续 append 进同一个列表，返回的帧数就与信封里的 `count`
不符，索引也不再是从 0 开始的连续序列。客户端（`chunk` 合并）据此判
"chunk index out of sequence" 并断连——所以这条必须由测试钉住。

用 monkeypatch 把 `SOFT_LIMIT_BYTES` 缩小来复现，避免真造 40 MiB 载荷。
"""

from __future__ import annotations

import json

import pytest

from wing.gateway import frames as frames_mod
from wing.gateway.frames import build_frames

#: 转义密度高的载荷（引号 / 反斜杠各膨胀 1 字节，切片按转义后长度算），
#: 真实帧数因此可以远超按原始字节估出来的 ``count_hint``。
HIGH_ESCAPE_CASES = {
    "quotes-5k": ('"' * 5_000, 1_000),
    "quotes-40k": ('"' * 40_000, 1_000),
    "backslashes-20k": ("\\" * 20_000, 1_000),
    "mixed-80k": ('"' * 40_000 + "x" * 40_000, 1_000),
}


@pytest.mark.parametrize(
    ("payload", "soft"), HIGH_ESCAPE_CASES.values(), ids=list(HIGH_ESCAPE_CASES)
)
def test_frames_match_their_envelope_count(
    payload: str, soft: int, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setattr(frames_mod, "SOFT_LIMIT_BYTES", soft)
    frames = build_frames(payload, "sync_session")
    envelopes = [json.loads(frame.text) for frame in frames]

    assert len(frames) > 10, "本用例要求跨过两位数帧数（重试轮的触发条件）"
    assert {e["count"] for e in envelopes} == {len(frames)}
    assert [e["index"] for e in envelopes] == list(range(len(frames)))
    assert "".join(e["data"] for e in envelopes) == payload
    assert len({e["id"] for e in envelopes}) == 1, "同一事件共享 chunk id"
    for frame in frames:
        assert frame.size <= soft
        assert frame.size == len(frame.text.encode("utf-8")), "size 必须精确"


def test_low_escape_payload_keeps_the_same_invariants(monkeypatch: pytest.MonkeyPatch):
    """对照：低转义密度的载荷（未必进重试轮）同样满足信封 / 序号 / 回拼不变量。"""
    monkeypatch.setattr(frames_mod, "SOFT_LIMIT_BYTES", 1_000)
    payload = "x" * 5_000
    frames = build_frames(payload, "notice")
    envelopes = [json.loads(frame.text) for frame in frames]
    assert {e["count"] for e in envelopes} == {len(frames)}
    assert [e["index"] for e in envelopes] == list(range(len(frames)))
    assert "".join(e["data"] for e in envelopes) == payload
