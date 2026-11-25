"""转义膨胀跨 count 位宽时重切，返回最终轮的连续信封。"""

from __future__ import annotations

import json
from unittest.mock import patch

import pytest

from wing.gateway import frames as frames_mod
from wing.gateway.frames import build_frames


@pytest.mark.parametrize("length", [5_000, 52_000])
def test_retry_only_packs_the_final_round(length, monkeypatch):
    monkeypatch.setattr(frames_mod, "SOFT_LIMIT_BYTES", 1_000)
    with (
        patch.object(frames_mod, "_split_raw", wraps=frames_mod._split_raw) as split,
        patch.object(frames_mod, "_frame", wraps=frames_mod._frame) as pack,
    ):
        frames = build_frames('"' * length, "sync_session")

    assert split.call_count == 2
    assert pack.call_count == len(frames)
    assert len(str(len(frames))) == len(str(length // 1_000 + 2)) + 1


def test_last_round_keeps_unavoidable_oversize_fragments(monkeypatch):
    monkeypatch.setattr(frames_mod, "SOFT_LIMIT_BYTES", 1)
    with patch.object(frames_mod, "_split_raw", wraps=frames_mod._split_raw) as split:
        frames = build_frames("中🙂", "sync_session")

    assert split.call_count == 4
    assert [json.loads(frame.text)["data"] for frame in frames] == ["中", "🙂"]
    assert all(frame.size == len(frame.text.encode()) > 1 for frame in frames)


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
