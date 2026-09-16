"""断言原语语义单测（tasks 4.8）——构造时间线，不起网关。

覆盖：游标推进与不重复匹配、``where`` 过滤（映射 / 谓词）、``expect`` 超时报告、
``expect_none`` 的负向路径与快速失败、``assert_never`` 含已消费事件、``assert_ordered``
的顺序分叉、``events`` 查询不推进游标、``within`` 的有限性校验、等待唤醒路径。

另含 ``driver/ws.py`` 的分片重组（``_chunk`` 信封）状态机测试：它与游标原语同类
——都是"构造输入 → 断言输出"的纯状态机，且是"时间线里只出现完整事件"这一前提的
唯一保证；把它放进本文件是为了守住"不为传输层单独开测试文件"的文件边界。
"""

from __future__ import annotations

import asyncio
import json
import math
from typing import Any

import pytest

from wing_probe.driver.ws import (
    ChunkLimits,
    Delivery,
    Reassembler,
    ReassemblyError,
    is_chunk_frame,
    parse_envelope,
)
from wing_probe.watch.expect import Watcher
from wing_probe.watch.report import ExpectationError
from wing_probe.watch.timeline import Event, FrameLog, Timeline

# ── 构造工具 ────────────────────────────────────────────────────


def build(*items: str | tuple[str, dict[str, Any]], name: str = "test") -> Timeline:
    """构造时间线：字符串 = 空 data 事件，元组 = (type, data)；时间步长 0.1s。"""
    timeline = Timeline(name, started_at=0.0)
    for index, item in enumerate(items):
        if isinstance(item, str):
            timeline.append(item, at=index * 0.1)
        else:
            timeline.append(item[0], item[1], at=index * 0.1)
    return timeline


def chunk_frame(
    payload: str,
    *,
    index: int,
    count: int,
    chunk_id: str = "7",
    of_type: str = "sync_session",
) -> str:
    """网关侧信封帧的线上形状（字段顺序与 ``gateway/frames.py::_head`` 一致）。"""
    return (
        '{"type":"_chunk"'
        f',"id":{json.dumps(chunk_id)}'
        f',"index":{index}'
        f',"count":{count}'
        f',"of_type":{json.dumps(of_type)}'
        f',"data":{json.dumps(payload)}'
        "}"
    )


# ── 时间线基础 ──────────────────────────────────────────────────


def test_timeline_records_events_with_relative_time() -> None:
    timeline = build(("turn_started", {}), ("text", {"content": "hi"}))

    assert len(timeline) == 2
    assert timeline.types() == ["turn_started", "text"]
    assert timeline.counts() == {"turn_started": 1, "text": 1}
    assert timeline.all()[1].data == {"content": "hi"}
    assert timeline.all()[1].at == pytest.approx(0.1)
    assert timeline.cursor == 0
    assert [e.index for e in timeline.pending()] == [0, 1]


def test_event_field_access_is_dict_like() -> None:
    timeline = build(
        ("tool_call", {"tool_name": "Bash", "session_id": "s1", "uuid": "u1"})
    )
    event = timeline.all()[0]

    assert event["tool_name"] == "Bash"
    assert event.get("missing", "fallback") == "fallback"
    assert "tool_name" in event
    assert event.session_id == "s1"
    assert event.uuid == "u1"
    assert str(event) == "#0 tool_call (+0.000s)"
    assert event.as_dict()["data"]["tool_name"] == "Bash"


def test_set_cursor_validates_range() -> None:
    timeline = build("text", "turn_result")

    timeline.set_cursor(2)
    assert timeline.cursor == 2
    assert timeline.consumed() == timeline.all()
    with pytest.raises(ValueError, match="out of range"):
        timeline.set_cursor(3)
    with pytest.raises(ValueError, match="out of range"):
        timeline.set_cursor(-1)


def test_frame_log_is_bounded_and_reports_drops() -> None:
    frames = FrameLog(max_frames=2)
    frames.add("a", at=0.1)
    frames.add("b", at=0.2)
    frames.add("c", at=0.3)

    assert [frame.text for frame in frames.tail(2)] == ["b", "c"]
    assert frames.dropped == 1
    assert len(frames) == 2
    assert frames.frames[0].chunk is False


# ── expect：正向 ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_expect_matches_first_event_and_advances_cursor() -> None:
    timeline = build("turn_started", "text", "turn_result")
    watcher = Watcher(timeline)

    event = await watcher.expect("text", within=0)

    assert event.type == "text"
    assert event.index == 1
    assert watcher.cursor == 2


@pytest.mark.asyncio
async def test_expect_does_not_rematch_consumed_events() -> None:
    timeline = build("text", "text")
    watcher = Watcher(timeline)

    first = await watcher.expect("text", within=0)
    second = await watcher.expect("text", within=0)

    assert (first.index, second.index) == (0, 1)
    with pytest.raises(ExpectationError):
        await watcher.expect("text", within=0)


@pytest.mark.asyncio
async def test_expect_accepts_several_types() -> None:
    timeline = build("text", "error")
    watcher = Watcher(timeline)

    event = await watcher.expect(["turn_result", "error"], within=0)

    assert event.type == "error"


@pytest.mark.asyncio
async def test_expect_filters_by_field_mapping() -> None:
    timeline = build(
        ("tool_call", {"tool_name": "Read"}),
        ("tool_call", {"tool_name": "Bash", "tool_call_id": "call_1"}),
    )
    watcher = Watcher(timeline)

    event = await watcher.expect("tool_call", where={"tool_name": "Bash"}, within=0)

    assert event["tool_call_id"] == "call_1"
    assert watcher.cursor == 2


@pytest.mark.asyncio
async def test_expect_filters_by_predicate() -> None:
    timeline = build(("text", {"content": "a"}), ("text", {"content": "bb"}))
    watcher = Watcher(timeline)

    event = await watcher.expect(
        "text", where=lambda e: len(e["content"]) == 2, within=0
    )

    assert event["content"] == "bb"


@pytest.mark.asyncio
async def test_expect_waits_for_an_event_appended_later() -> None:
    timeline = Timeline("test", started_at=0.0)
    watcher = Watcher(timeline, default_within=2.0)

    async def append_later() -> None:
        await asyncio.sleep(0.01)
        timeline.append("turn_result", {"subtype": "success"}, at=0.5)

    task = asyncio.create_task(append_later())
    event = await watcher.expect("turn_result")
    await task

    assert event.at == pytest.approx(0.5)
    assert watcher.cursor == 1


@pytest.mark.asyncio
async def test_expect_timeout_reports_timeline_and_frames() -> None:
    timeline = build("turn_started")
    frames = FrameLog()
    frames.add('{"type":"turn_started"}', at=0.0)
    frames.add('{"type":"text","content":"hi"}', at=0.05)
    watcher = Watcher(timeline, frames=frames, dump_path="/tmp/probe/artifacts")

    with pytest.raises(ExpectationError) as excinfo:
        await watcher.expect("turn_result", within=0.02)

    report = str(excinfo.value)
    assert isinstance(excinfo.value, AssertionError)
    assert report == excinfo.value.report
    assert "ExpectationError: expect turn_result (within 0.02s)" in report
    assert "expected: turn_result to appear at or after the cursor" in report
    assert "timeline 'test' · cursor 0 · 1 event(s) total, 1 after cursor" in report
    assert "[  0] +0.000s  turn_started" in report
    assert "--- raw frame tail: last 2 of 2 recorded ---" in report
    assert '{"type":"text","content":"hi"}' in report
    assert "--- artifacts: /tmp/probe/artifacts ---" in report
    # 游标不动：超时不得吞掉已有事件
    assert watcher.cursor == 0


@pytest.mark.asyncio
async def test_expect_reports_consumed_events_as_none_after_cursor() -> None:
    timeline = build("text")
    watcher = Watcher(timeline)
    await watcher.expect("text", within=0)

    with pytest.raises(ExpectationError) as excinfo:
        await watcher.expect("turn_result", within=0.01)

    report = str(excinfo.value)
    assert "(no event after the cursor)" in report
    assert "1 earlier event(s) already consumed" in report


@pytest.mark.asyncio
async def test_expect_uses_default_within_when_omitted() -> None:
    watcher = Watcher(Timeline("test", started_at=0.0), default_within=0.02)

    with pytest.raises(ExpectationError) as excinfo:
        await watcher.expect("text")

    assert "within 0.02s" in str(excinfo.value)


# ── expect_none：负向 ───────────────────────────────────────────


@pytest.mark.asyncio
async def test_expect_none_passes_when_nothing_appears() -> None:
    timeline = Timeline("test", started_at=0.0)
    watcher = Watcher(timeline)

    async def append_other() -> None:
        await asyncio.sleep(0.005)
        timeline.append("text", {"content": "hi"}, at=0.1)

    task = asyncio.create_task(append_other())
    await watcher.expect_none(["error"], within=0.05)
    await task

    # 窗口内出现的**其他**事件不受影响，也不推进游标
    assert watcher.cursor == 0
    assert await watcher.expect("text", within=0) is not None


@pytest.mark.asyncio
async def test_expect_none_fails_fast_on_match() -> None:
    timeline = build("turn_started")
    watcher = Watcher(timeline)

    async def append_error() -> None:
        await asyncio.sleep(0.005)
        timeline.append("error", {"message": "boom"}, at=0.2)

    task = asyncio.create_task(append_error())
    with pytest.raises(ExpectationError) as excinfo:
        await watcher.expect_none(["error"], within=5.0)
    await task

    report = str(excinfo.value)
    assert "expect no error within 5s" in report
    assert "first divergence: unexpected 'error' event at index 1 (+0.200s)" in report
    assert '"message": "boom"' in report
    assert watcher.cursor == 0


@pytest.mark.asyncio
async def test_expect_none_sees_consumed_events_as_history_only() -> None:
    # 游标之前的事件已被断言消费，不参与负向窗口
    timeline = build("error", "text")
    watcher = Watcher(timeline)
    await watcher.expect("error", within=0)  # 消费掉它

    await watcher.expect_none(["error"], within=0)

    assert watcher.cursor == 1


@pytest.mark.asyncio
async def test_expect_none_flags_unconsumed_events_after_cursor() -> None:
    timeline = build("text", "error")
    watcher = Watcher(timeline)

    with pytest.raises(ExpectationError) as excinfo:
        await watcher.expect_none(["error"], within=0)

    assert "unexpected 'error'" in str(excinfo.value)


# ── assert_never ────────────────────────────────────────────────


def test_assert_never_passes_when_absent() -> None:
    watcher = Watcher(build("text", "turn_result"))

    watcher.assert_never("error")
    watcher.assert_never(["compaction_failed", "timeout"])


def test_assert_never_sees_consumed_events() -> None:
    timeline = build("text", "error")
    watcher = Watcher(timeline)
    timeline.set_cursor(2)  # 全部消费

    with pytest.raises(ExpectationError) as excinfo:
        watcher.assert_never("error")

    report = str(excinfo.value)
    assert "ExpectationError: assert_never error" in report
    assert "whole timeline inspected, consumed events included" in report
    assert "first divergence: 'error' occurred at index 1 (+0.100s)" in report


def test_assert_never_honours_where_and_type_sets() -> None:
    timeline = build(("error", {"recoverable": True}))
    watcher = Watcher(timeline)

    # 谓词把它排除掉：可恢复的错误不算"必须不看到"
    watcher.assert_never("error", where={"recoverable": False})
    watcher.assert_never(["timeout", "compaction_failed"])

    with pytest.raises(ExpectationError) as excinfo:
        watcher.assert_never("error")
    assert "index 0" in str(excinfo.value)


# ── assert_ordered ──────────────────────────────────────────────


def test_assert_ordered_allows_interleaving_and_advances_cursor() -> None:
    timeline = build(
        "turn_started",
        "tool_call",
        "text",
        "tool_call_result",
        "text",
        "turn_result",
    )
    watcher = Watcher(timeline)

    watcher.assert_ordered(["tool_call", "tool_call_result", "turn_result"])

    assert watcher.cursor == 6


def test_assert_ordered_only_scans_from_cursor() -> None:
    timeline = build("tool_call", "tool_call_result")
    watcher = Watcher(timeline)
    watcher.assert_ordered(["tool_call", "tool_call_result"])
    timeline.append("tool_call", at=0.5)

    watcher.assert_ordered(["tool_call"])

    assert watcher.cursor == 3


def test_assert_ordered_accepts_an_explicit_window() -> None:
    # "等整轮结束后再断言这一轮的顺序"：expect 会推进游标，用 since= 取回窗口
    timeline = build("turn_started", "tool_call", "tool_call_result", "turn_result")
    watcher = Watcher(timeline)
    start = watcher.cursor
    watcher.timeline.set_cursor(4)  # 模拟 expect("turn_result") 之后

    watcher.assert_ordered(["tool_call", "tool_call_result"], since=start)

    # 命中在游标之前：断言可回看，但游标绝不后退（已消费的事件不得重新暴露）
    assert watcher.cursor == 4


def test_assert_ordered_reports_first_divergence() -> None:
    timeline = build("tool_call", "text", "turn_result")
    watcher = Watcher(timeline)

    with pytest.raises(ExpectationError) as excinfo:
        watcher.assert_ordered(["tool_call", "tool_call_result", "turn_result"])

    report = str(excinfo.value)
    assert (
        "ExpectationError: assert_ordered ['tool_call', 'tool_call_result', 'turn_result']"
        in report
    )
    assert "expected order: 'tool_call' → 'tool_call_result' → 'turn_result'" in report
    assert "first divergence: position 1 expected 'tool_call_result'" in report
    assert "matched so far: 'tool_call' (index 0, +0.000s)" in report
    assert "skipped instead: [text@1, turn_result@2]" in report
    assert watcher.cursor == 0


def test_assert_ordered_reports_empty_remainder() -> None:
    timeline = build("tool_call")
    watcher = Watcher(timeline)

    with pytest.raises(ExpectationError) as excinfo:
        watcher.assert_ordered(["tool_call", "tool_call_result"])

    assert "skipped instead: (no further event at all)" in str(excinfo.value)


# ── 查询 ────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_events_query_covers_whole_timeline_without_moving_cursor() -> None:
    timeline = build("text", ("tool_call", {"tool_name": "Bash"}), "text")
    watcher = Watcher(timeline)
    await watcher.expect("text", within=0)

    assert len(watcher.events()) == 3
    assert [e.index for e in watcher.events("text")] == [0, 2]
    assert [e.index for e in watcher.events(where={"tool_name": "Bash"})] == [1]
    assert watcher.cursor == 1
    assert [e.index for e in watcher.pending()] == [1, 2]


# ── 有界等待校验 ────────────────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.parametrize("within", [math.inf, -1.0, math.nan])
async def test_unbounded_or_negative_within_is_rejected(within: float) -> None:
    watcher = Watcher(build("text"))

    with pytest.raises(ValueError, match="within"):
        await watcher.expect("text", within=within)


@pytest.mark.asyncio
async def test_zero_within_is_allowed_and_non_blocking() -> None:
    watcher = Watcher(build("text"))

    event = await watcher.expect("text", within=0)

    assert event.type == "text"


def test_normalize_types_rejects_empty() -> None:
    watcher = Watcher(build("text"))

    with pytest.raises(ValueError, match="at least one event type"):
        watcher.assert_never([])
    with pytest.raises(ValueError, match="non-empty strings"):
        watcher.assert_never([""])


# ── 分片重组（driver/ws.py::Reassembler） ───────────────────────


def test_reassembler_passes_normal_frames_through() -> None:
    reassembler = Reassembler()

    deliveries = reassembler.on_text('{"type":"text"}', at=0.1)

    assert deliveries is not None
    assert [d.text for d in deliveries] == ['{"type":"text"}']
    assert deliveries[0].frames == 1


def test_reassembler_merges_chunked_event() -> None:
    reassembler = Reassembler()
    payload = '{"type":"sync_session","payload":"' + "x" * 30 + '"}'
    parts = [payload[:20], payload[20:40], payload[40:]]

    assert reassembler.on_text(chunk_frame(parts[0], index=0, count=3), at=0.1) is None
    assert reassembler.on_text(chunk_frame(parts[1], index=1, count=3), at=0.2) is None
    assert reassembler.assembling is True

    deliveries = reassembler.on_text(chunk_frame(parts[2], index=2, count=3), at=0.3)

    assert deliveries is not None
    assert len(deliveries) == 1
    assert deliveries[0].text == payload
    assert deliveries[0].frames == 3
    assert deliveries[0].at == pytest.approx(0.3)  # 闭合帧的到达时刻
    assert reassembler.assembling is False


def test_reassembler_releases_buffered_frames_after_the_event() -> None:
    reassembler = Reassembler()
    live_delta = '{"type":"text","content":"hi"}'

    assert reassembler.on_text(chunk_frame("ab", index=0, count=2), at=0.1) is None
    # 窗口打开期间的普通帧：原样缓冲，不解析、不投递（保序）
    assert reassembler.on_text(live_delta, at=0.15) is None

    deliveries = reassembler.on_text(chunk_frame("cd", index=1, count=2), at=0.2)

    assert deliveries is not None
    assert [d.text for d in deliveries] == ["abcd", live_delta]
    assert [d.frames for d in deliveries] == [2, 1]
    assert deliveries[1].at == pytest.approx(0.15)
    # 放行后窗口与缓冲都已清空：后续普通帧立刻直通
    assert reassembler.assembling is False
    assert reassembler.on_text(live_delta, at=0.3) == [Delivery(live_delta, 0.3)]


@pytest.mark.parametrize(
    ("frames", "match"),
    [
        ([chunk_frame("ab", index=1, count=2)], "without a preceding index 0"),
        (
            [
                chunk_frame("ab", index=0, count=3),
                chunk_frame("cd", index=2, count=3),
            ],
            "out of sequence",
        ),
        (
            [
                chunk_frame("ab", index=0, count=3),
                chunk_frame("ab", index=0, count=3),
            ],
            "duplicate chunk index",
        ),
        (
            [
                chunk_frame("ab", index=0, count=2),
                chunk_frame("cd", index=1, count=2, chunk_id="8"),
            ],
            "chunk id changed mid-window",
        ),
        ([chunk_frame("ab", index=0, count=1)], "out of range"),
        ([chunk_frame("ab", index=0, count=4096)], "out of range"),
    ],
)
def test_reassembler_rejects_protocol_violations(frames: list[str], match: str) -> None:
    reassembler = Reassembler()

    with pytest.raises(ReassemblyError, match=match):
        for frame in frames:
            reassembler.on_text(frame, at=0.1)


def test_reassembler_enforces_buffer_cap() -> None:
    reassembler = Reassembler(ChunkLimits(max_buffered_bytes=8))

    with pytest.raises(ReassemblyError, match="buffer exceeded"):
        reassembler.on_text(chunk_frame("x" * 32, index=0, count=2), at=0.1)


def test_reassembler_tracks_idle_deadline() -> None:
    reassembler = Reassembler(ChunkLimits(idle_timeout=30.0))

    assert reassembler.deadline() is None
    assert reassembler.seconds_until_deadline(1.0) is None
    assert reassembler.timeout_detail() == "no reassembly window was open"

    reassembler.on_text(chunk_frame("ab", index=0, count=2), at=1.0)
    assert reassembler.seconds_until_deadline(1.0) == pytest.approx(30.0)
    assert reassembler.seconds_until_deadline(40.0) == 0.0
    assert reassembler.on_text(chunk_frame("cd", index=1, count=2), at=40.0) is not None
    assert reassembler.assembling is False

    reassembler.on_text(chunk_frame("ab", index=0, count=3), at=100.0)
    detail = reassembler.timeout_detail()
    assert "stalled" in detail
    assert "1/3 fragment(s) received" in detail


def test_chunk_sniffing_and_malformed_envelopes() -> None:
    assert is_chunk_frame(chunk_frame("x", index=0, count=2)) is True
    assert is_chunk_frame('{"type":"text","content":"_chunk"}') is False
    assert parse_envelope('{"type":"text","content":"_chunk"}') is None

    assert parse_envelope('{"type":"text"}') is None
    envelope = parse_envelope(
        chunk_frame("abc", index=1, count=2, of_type="sync_session")
    )
    assert envelope is not None
    assert (envelope.index, envelope.count, envelope.data) == (1, 2, "abc")
    assert envelope.of_type == "sync_session"

    with pytest.raises(ReassemblyError, match="malformed chunk envelope"):
        parse_envelope('{"type":"_chunk","id":"1","index":"0","count":2,"data":"x"}')


def test_event_matches_supports_mapping_subset() -> None:
    event = Event(index=0, type="tool_call", data={"tool_name": "Bash", "n": 1}, at=0.0)

    assert event.matches(("tool_call",), {"tool_name": "Bash"}) is True
    assert event.matches(("tool_call",), {"tool_name": "Read"}) is False
    assert event.matches(("tool_call",), {"missing": 1}) is False
    assert event.matches(None, None) is True
    assert event.matches(("text",), None) is False
