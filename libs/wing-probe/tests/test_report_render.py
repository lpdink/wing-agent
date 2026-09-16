"""失败报告渲染单测（tasks 4.9）—— 纯渲染，不起网关。

报告是本基础设施的"用户界面"：断言失败时它是唯一的证据链。这里逐节对账
（期望描述 / 聚焦事件 / 游标后时间线 / 原始帧尾部 / 现场转储路径），并覆盖
截断、空时间线、帧日志缺省等边角形态。
"""

from __future__ import annotations

from typing import Any

import pytest

from wing_probe.watch.expect import Watcher
from wing_probe.watch.report import (
    KIND_EXPECT,
    KIND_EXPECT_NONE,
    KIND_NEVER,
    KIND_ORDERED,
    EVENT_SNIPPET,
    Expectation,
    ExpectationError,
    describe_where,
    format_at,
    format_within,
    render_event,
    render_frames,
    render_report,
    render_timeline,
    truncate,
)
from wing_probe.watch.timeline import Event, FrameLog, Timeline


def build(*items: str | tuple[str, dict[str, Any]], name: str = "test") -> Timeline:
    timeline = Timeline(name, started_at=0.0)
    for index, item in enumerate(items):
        if isinstance(item, str):
            timeline.append(item, at=index * 0.1)
        else:
            timeline.append(item[0], item[1], at=index * 0.1)
    return timeline


# ── 小工具 ──────────────────────────────────────────────────────


def test_truncate_marks_the_omitted_length() -> None:
    assert truncate("abc", 5) == "abc"
    assert truncate("abcdef", 3) == "abc…(+3 chars)"


def test_time_and_duration_formatting() -> None:
    assert format_at(0.5123) == "+0.512s"
    assert format_within(5.0) == "5s"
    assert format_within(0.02) == "0.02s"
    assert format_within(None) == "unbounded"


def test_describe_where_renders_all_shapes() -> None:
    assert describe_where(None) is None
    assert describe_where({"tool_name": "Bash"}) == 'fields {"tool_name": "Bash"}'

    def is_bash(event: Event) -> bool:
        return event["tool_name"] == "Bash"

    assert describe_where(is_bash) == "predicate is_bash()"
    assert describe_where(lambda event: True) == "predicate <lambda>()"


# ── 期望描述 ────────────────────────────────────────────────────


def test_expectation_renders_each_kind() -> None:
    expect = Expectation(
        kind=KIND_EXPECT,
        types=("turn_result",),
        within=5.0,
        where="fields {}",
    )
    assert expect.subject == "turn_result"
    assert expect.summary() == "expect turn_result (within 5s)"
    assert expect.render() == (
        "expected: turn_result to appear at or after the cursor (waited 5s)\n"
        "  where: fields {}"
    )

    none = Expectation(kind=KIND_EXPECT_NONE, types=("error",), within=1.0)
    assert none.summary() == "expect no error within 1s"
    assert none.render() == "expected: no error during the next 1s"

    never = Expectation(kind=KIND_NEVER, types=("error", "timeout"))
    assert never.subject == "['error', 'timeout']"
    assert never.summary() == "assert_never ['error', 'timeout']"
    assert "whole timeline inspected" in never.render()

    ordered = Expectation(
        kind=KIND_ORDERED,
        types=("tool_call", "tool_call_result"),
        detail="first divergence: position 1 expected 'tool_call_result'",
    )
    assert ordered.summary() == "assert_ordered ['tool_call', 'tool_call_result']"
    rendered = ordered.render()
    assert "expected order: 'tool_call' → 'tool_call_result'" in rendered
    assert "interleaved" in rendered
    assert "first divergence: position 1" in rendered


# ── 分节渲染 ────────────────────────────────────────────────────


def test_render_event_shows_index_time_type_and_fields() -> None:
    timeline = build(("tool_call", {"tool_name": "Bash", "n": 1}))
    event = timeline.all()[0]

    assert (
        render_event(event)
        == '  [  0] +0.000s  tool_call  {"n": 1, "tool_name": "Bash"}'
    )


def test_render_event_marks_reassembled_payloads() -> None:
    timeline = Timeline("test", started_at=0.0)
    timeline.append("sync_session", {"session_id": "s1"}, at=0.2, frames=3)

    assert "(x3 frames)" in render_event(timeline.all()[0])


def test_render_event_truncates_long_fields() -> None:
    event = Event(index=2, type="text", data={"content": "x" * 400}, at=1.0)

    line = render_event(event, snippet=20)

    assert '"content"' in line
    assert "chars)" in line
    assert line.count("x") <= 20
    assert len(line) < 100


def test_render_timeline_lists_events_after_the_cursor() -> None:
    timeline = build("turn_started", ("text", {"content": "hi"}), "turn_result")
    timeline.set_cursor(1)

    rendered = render_timeline(timeline)

    assert "timeline 'test' · cursor 1 · 3 event(s) total, 2 after cursor" in rendered
    assert "[  0] +0.000s  turn_started" not in rendered  # 已消费的不再刷屏
    assert '[  1] +0.100s  text  {"content": "hi"}' in rendered
    assert "[  2] +0.200s  turn_result" in rendered


def test_render_timeline_handles_empty_and_truncated_cases() -> None:
    empty = render_timeline(build())
    assert "(no event after the cursor)" in empty

    consumed = build("text")
    consumed.set_cursor(1)
    assert "1 earlier event(s) already consumed" in render_timeline(consumed)

    busy = build(*[f"e{i}" for i in range(5)])
    rendered = render_timeline(busy, limit=2)
    assert "… 3 more event(s)" in rendered
    assert "e1" in rendered and "e4" not in rendered


def test_render_timeline_uses_event_snippet_default() -> None:
    timeline = build(("text", {"content": "y" * (EVENT_SNIPPET * 2)}))

    rendered = render_timeline(timeline)

    assert "chars)" in rendered


def test_render_frames_reports_tail_and_drops() -> None:
    frames = FrameLog(max_frames=2)
    frames.add('{"type":"text","content":"hi"}', at=0.1)
    frames.add('{"type":"_chunk","id":"7","index":0,"count":3}', at=0.2, chunk=True)
    frames.add('{"type":"_chunk","id":"7","index":1,"count":3}', at=0.3, chunk=True)

    rendered = render_frames(frames)

    assert "raw frame tail: last 2 of 2 recorded, 1 dropped" in rendered
    assert "+0.200s  chunk" in rendered
    assert "+0.100s" not in rendered
    assert '"index":1' in rendered


def test_render_frames_handles_missing_and_empty_logs() -> None:
    assert "not recorded" in render_frames(None)
    assert "(no frames recorded)" in render_frames(FrameLog())


def test_render_frames_truncates_long_frames() -> None:
    frames = FrameLog()
    frames.add('{"data":"' + "z" * 900 + '"}', at=0.1)

    rendered = render_frames(frames, snippet=40)

    assert '{"data":' in rendered
    assert "chars)" in rendered
    assert rendered.count("z") <= 40


# ── 报告组装 ────────────────────────────────────────────────────


def test_render_report_contains_every_section() -> None:
    timeline = build("turn_started", ("error", {"message": "boom"}))
    frames = FrameLog()
    frames.add('{"type":"error","message":"boom"}', at=0.1)
    focus = timeline.all()[1]
    expectation = Expectation(
        kind=KIND_EXPECT,
        types=("turn_result",),
        within=5.0,
        where="fields {}",
        detail="first divergence: nothing matched",
    )

    report = render_report(
        expectation, timeline=timeline, frames=frames, focus=focus, dump_path="/tmp/a"
    )

    assert report.startswith("ExpectationError: expect turn_result (within 5s)")
    assert (
        "expected: turn_result to appear at or after the cursor (waited 5s)" in report
    )
    assert "  where: fields {}" in report
    assert "  first divergence: nothing matched" in report
    assert "the event that broke the expectation (index 1, +0.100s)" in report
    assert '[  1] +0.100s  error  {"message": "boom"}' in report
    assert (
        "--- timeline 'test' · cursor 0 · 2 event(s) total, 2 after cursor ---"
        in report
    )
    assert "--- raw frame tail: last 1 of 1 recorded ---" in report
    assert report.endswith("--- artifacts: /tmp/a ---")


def test_render_report_omits_optional_sections() -> None:
    timeline = build("text")
    report = render_report(
        Expectation(kind=KIND_NEVER, types=("error",)), timeline=timeline
    )

    assert "the event that broke" not in report
    assert "artifacts" not in report
    assert "not recorded" in report


# ── 异常对象 ────────────────────────────────────────────────────


def test_expectation_error_carries_the_rendered_report() -> None:
    timeline = build(("error", {"message": "boom"}))
    expectation = Expectation(kind=KIND_NEVER, types=("error",))
    focus = timeline.all()[0]

    error = ExpectationError(expectation, timeline=timeline, focus=focus)

    assert isinstance(error, AssertionError)
    assert error.expectation is expectation
    assert error.timeline is timeline
    assert error.focus is focus
    assert str(error) == error.report
    assert "ExpectationError: assert_never error" in error.report


@pytest.mark.asyncio
async def test_watcher_failures_use_the_shared_renderer() -> None:
    timeline = build("text")
    frames = FrameLog()
    frames.add('{"type":"text"}', at=0.0)
    watcher = Watcher(timeline, frames=frames, dump_path="/tmp/artifacts")

    with pytest.raises(ExpectationError) as excinfo:
        await watcher.expect("turn_result", within=0)

    report = excinfo.value.report
    assert report is not None
    assert "raw frame tail: last 1 of 1 recorded" in report
    assert "artifacts: /tmp/artifacts" in report
    assert excinfo.value.expectation.kind == KIND_EXPECT
    assert excinfo.value.expectation.where is None
