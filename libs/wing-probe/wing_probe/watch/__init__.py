"""观察者子包：事件时间线（``timeline``）+ 游标断言原语（``expect``）+ 失败报告（``report``）。

典型用法（场景内）：

    turn = await session.watch.expect("turn_result", within=10.0)
    session.watch.assert_ordered(["tool_call", "tool_call_result"])
    session.watch.assert_never(["error"])
"""

from wing_probe.watch.expect import DEFAULT_WITHIN, Watcher
from wing_probe.watch.report import (
    KIND_EXPECT,
    KIND_EXPECT_NONE,
    KIND_NEVER,
    KIND_ORDERED,
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
from wing_probe.watch.timeline import (
    DEFAULT_MAX_FRAMES,
    DEFAULT_MAX_FRAME_BYTES,
    Event,
    Frame,
    FrameLog,
    Timeline,
    Where,
    matches,
    normalize_types,
)

__all__ = [
    "DEFAULT_MAX_FRAMES",
    "DEFAULT_MAX_FRAME_BYTES",
    "DEFAULT_WITHIN",
    "KIND_EXPECT",
    "KIND_EXPECT_NONE",
    "KIND_NEVER",
    "KIND_ORDERED",
    "Event",
    "Expectation",
    "ExpectationError",
    "Frame",
    "FrameLog",
    "Timeline",
    "Watcher",
    "Where",
    "describe_where",
    "format_at",
    "format_within",
    "matches",
    "normalize_types",
    "render_event",
    "render_frames",
    "render_report",
    "render_timeline",
    "truncate",
]
