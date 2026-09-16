"""失败报告渲染（design D4/D8，spec「失败报告与现场转储」）。

报告要在**不重跑**的情况下回答三个问题：期望什么、实际发生了什么、第一处分叉在哪。
因此渲染是纯函数（输入时间线 + 期望，输出文本），可独立单测——`ExpectationError`
只负责把它们打包成异常。

版式（各节按需出现）：

    ExpectationError: expect turn_result (within 5s)
      where: tool_name == 'Bash'
    --- timeline 'session 3f2a' · cursor 4 · 9 event(s) total, 5 after cursor ---
      [ 4] +0.512s  tool_call  {"tool_name": "Bash", …}
      …
    --- raw frame tail: last 2 of 9 (3 dropped) ---
      +0.501s  {"type":"tool_call",…}
    --- artifacts: /tmp/probe/artifacts ---
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from wing_probe.watch.timeline import Event, Frame, FrameLog, Timeline, Where

#: 断言种类（``Expectation.kind``）。
KIND_EXPECT = "expect"
KIND_EXPECT_NONE = "expect_none"
KIND_NEVER = "assert_never"
KIND_ORDERED = "assert_ordered"

#: 渲染上限（报告要可读：截断而非刷屏；完整数据在时间线与现场转储里）。
DEFAULT_EVENT_LIMIT = 40
DEFAULT_FRAME_LIMIT = 8
EVENT_SNIPPET = 200
FRAME_SNIPPET = 400


def truncate(text: str, limit: int) -> str:
    """超长截断（附原始长度，报告可读且不淹没上下文）。"""
    if len(text) <= limit:
        return text
    return f"{text[:limit]}…(+{len(text) - limit} chars)"


def format_at(at: float) -> str:
    """相对时间戳（``+0.512s``）——所有时间线输出共用同一形状。"""
    return f"+{at:.3f}s"


def format_within(within: float | None) -> str:
    """时限渲染（``5s`` / ``0.01s``；未设时限不该出现，渲染成 ``unbounded``）。"""
    return "unbounded" if within is None else f"{within:g}s"


def describe_where(where: Where | None) -> str | None:
    """谓词的人类可读描述（``None`` 表示不过滤）。"""
    if where is None:
        return None
    if isinstance(where, Mapping):
        try:
            body = json.dumps(dict(where), ensure_ascii=False, sort_keys=True)
        except (TypeError, ValueError):  # pragma: no cover - 不可序列化的构造
            body = repr(dict(where))
        return f"fields {body}"
    name = getattr(where, "__name__", None)
    return f"predicate {name}()" if name else f"predicate {where!r}"


@dataclass(frozen=True, slots=True)
class Expectation:
    """被违反的期望：种类 + 目标类型 + 时限 + 谓词 + 细节（分叉 / 越界事件）。"""

    kind: str
    types: tuple[str, ...]
    within: float | None = None
    where: str | None = None
    detail: str | None = None

    @property
    def subject(self) -> str:
        """目标类型集合的渲染（单元素退化为裸类型名）。"""
        if len(self.types) == 1:
            return self.types[0]
        return "[" + ", ".join(repr(t) for t in self.types) + "]"

    def summary(self) -> str:
        """一行摘要（异常首行 / 报告标题）。"""
        if self.kind == KIND_EXPECT:
            return f"expect {self.subject} (within {format_within(self.within)})"
        if self.kind == KIND_EXPECT_NONE:
            return f"expect no {self.subject} within {format_within(self.within)}"
        if self.kind == KIND_NEVER:
            return f"assert_never {self.subject}"
        if self.kind == KIND_ORDERED:
            return f"assert_ordered {self.subject}"
        return f"{self.kind} {self.subject}"

    def render(self) -> str:
        """期望分节：期望描述 + 谓词 + 第一处分叉。"""
        lines: list[str] = []
        if self.kind == KIND_EXPECT:
            lines.append(
                f"expected: {self.subject} to appear at or after the cursor "
                f"(waited {format_within(self.within)})"
            )
        elif self.kind == KIND_EXPECT_NONE:
            lines.append(
                f"expected: no {self.subject} during the next "
                f"{format_within(self.within)}"
            )
        elif self.kind == KIND_NEVER:
            lines.append(
                f"expected: {self.subject} to never appear "
                f"(whole timeline inspected, consumed events included)"
            )
        elif self.kind == KIND_ORDERED:
            lines.append(
                "expected order: "
                + " → ".join(repr(t) for t in self.types)
                + " (other events may be interleaved)"
            )
        else:  # pragma: no cover - 未知种类只影响文案
            lines.append(f"expected: {self.subject}")
        if self.where is not None:
            lines.append(f"  where: {self.where}")
        if self.detail is not None:
            lines.append(f"  {self.detail}")
        return "\n".join(lines)


class ExpectationError(AssertionError):
    """断言原语失败（超时 / 负向命中 / 顺序分叉）。

    继承 ``AssertionError``：pytest 把它当断言失败报告，而不是"测试自身出错"。
    渲染结果同时挂在 ``report`` 上（异常参数里只有完整报告，便于日志直接看）。
    """

    def __init__(
        self,
        expectation: Expectation,
        *,
        timeline: Timeline,
        frames: FrameLog | Sequence[Frame] | None = None,
        focus: Event | None = None,
        dump_path: str | None = None,
        timeline_cursor: int | None = None,
    ) -> None:
        self.expectation = expectation
        self.timeline = timeline
        self.focus = focus
        self.report = render_report(
            expectation,
            timeline=timeline,
            frames=frames,
            focus=focus,
            dump_path=dump_path,
            timeline_cursor=timeline_cursor,
        )
        super().__init__(self.report)


def render_event(event: Event, *, snippet: int = EVENT_SNIPPET) -> str:
    """时间线里的一行：``[ 4] +0.512s  tool_call  {…}``。"""
    try:
        body = json.dumps(event.data, ensure_ascii=False, sort_keys=True, default=str)
    except (TypeError, ValueError):  # pragma: no cover - default=str 已兜底
        body = repr(event.data)
    marker = f" (x{event.frames} frames)" if event.frames > 1 else ""
    return (
        f"  [{event.index:>3}] {format_at(event.at)}  {event.type}{marker}  "
        f"{truncate(body, snippet)}"
    )


def render_timeline(
    timeline: Timeline,
    *,
    cursor: int | None = None,
    limit: int = DEFAULT_EVENT_LIMIT,
    snippet: int = EVENT_SNIPPET,
) -> str:
    """时间线分节：游标之后的全部事件（含相对时间戳与原始字段）。"""
    start = timeline.cursor if cursor is None else cursor
    events = timeline.all()
    after = events[start:]
    header = (
        f"--- timeline {timeline.name!r} · cursor {start} · "
        f"{len(events)} event(s) total, {len(after)} after cursor ---"
    )
    if not after:
        consumed = (
            f" ({len(events)} earlier event(s) already consumed — see events())"
            if events
            else ""
        )
        return f"{header}\n  (no event after the cursor){consumed}"
    lines = [header]
    for event in after[:limit]:
        lines.append(render_event(event, snippet=snippet))
    if len(after) > limit:
        lines.append(f"  … {len(after) - limit} more event(s)")
    return "\n".join(lines)


def render_frames(
    frames: FrameLog | Sequence[Frame] | None,
    *,
    limit: int = DEFAULT_FRAME_LIMIT,
    snippet: int = FRAME_SNIPPET,
    dropped: int | None = None,
) -> str:
    """原始帧分节：最后 N 条线上帧（未重组的原貌，用于核对协议层事实）。"""
    if frames is None:
        return "--- raw frame tail: (not recorded) ---"
    log = frames if isinstance(frames, FrameLog) else None
    items = log.tail(limit) if log is not None else list(frames)[-limit:]
    total = len(frames)
    dropped_count = log.dropped if log is not None else (dropped or 0)
    dropped_note = f", {dropped_count} dropped" if dropped_count else ""
    header = (
        f"--- raw frame tail: last {len(items)} of {total} recorded{dropped_note} ---"
    )
    if not items:
        return f"{header}\n  (no frames recorded)"
    lines = [header]
    for frame in items:
        kind = "chunk" if frame.chunk else "event"
        lines.append(
            f"  {format_at(frame.at)}  {kind:<5}  {truncate(frame.text, snippet)}"
        )
    return "\n".join(lines)


def render_report(
    expectation: Expectation,
    *,
    timeline: Timeline,
    frames: FrameLog | Sequence[Frame] | None = None,
    focus: Event | None = None,
    dump_path: str | None = None,
    timeline_cursor: int | None = None,
    event_limit: int = DEFAULT_EVENT_LIMIT,
    frame_limit: int = DEFAULT_FRAME_LIMIT,
    snippet: int = EVENT_SNIPPET,
    frame_snippet: int = FRAME_SNIPPET,
) -> str:
    """完整失败报告（期望 → 聚焦事件 → 时间线 → 原始帧 → 现场转储路径）。

    ``timeline_cursor`` 覆盖时间线分节的起点（默认 = 时间线当前游标）。
    "在预算之外的路径上失败"（如 ``Session.chat`` 撞上 ``error`` 事件、此时游标
    已被推进到失败点之后）需要它来把**失败前的上下文**也渲染进报告。
    """
    sections = [f"ExpectationError: {expectation.summary()}", expectation.render()]
    if focus is not None:
        sections.append(
            "--- the event that broke the expectation "
            f"(index {focus.index}, {format_at(focus.at)}) ---\n"
            + render_event(focus, snippet=snippet)
        )
    sections.append(
        render_timeline(
            timeline, cursor=timeline_cursor, limit=event_limit, snippet=snippet
        )
    )
    sections.append(render_frames(frames, limit=frame_limit, snippet=frame_snippet))
    if dump_path is not None:
        sections.append(f"--- artifacts: {dump_path} ---")
    return "\n".join(sections)
