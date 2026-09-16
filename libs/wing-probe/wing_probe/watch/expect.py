"""断言原语 —— 游标模型（design D4，spec「事件时间线断言原语」）。

| API | 语义 | 游标 |
|---|---|---|
| ``await watch.expect(type, where=?, within=?)`` | 游标后首个匹配（无则等待到 ``within``） | 命中即推进 |
| ``await watch.expect_none(types, where=?, within=?)`` | **未来**窗口内不得出现 | 不动 |
| ``watch.assert_never(type, where=?)`` | **全时间线**（含已消费）从未出现 | 不动 |
| ``watch.assert_ordered(types, where=?)`` | 游标后按序出现（允许夹杂其他事件） | 推进到最后命中处 |
| ``watch.events(type=?, where=?)`` | 全量查询 | 不动 |

两条硬规则：

- **禁止无界等待**：``within`` 缺省取 ``default_within``（默认 5s），显式传入也必须
  是有限值（``inf`` / ``None`` 一律拒绝）；``within=0`` = 只看当下已有；
- **失败即报告**：超时 / 负向命中 / 顺序分叉都抛 ``ExpectationError``，报告含期望
  描述、游标后的实际时间线、原始帧尾部（见 ``wing_probe.watch.report``）。
"""

from __future__ import annotations

import math
from collections.abc import Iterable, Sequence
from dataclasses import replace

from wing_probe.watch.report import (
    KIND_EXPECT,
    KIND_EXPECT_NONE,
    KIND_NEVER,
    KIND_ORDERED,
    Expectation,
    ExpectationError,
    describe_where,
    format_at,
)
from wing_probe.watch.timeline import (
    Event,
    Frame,
    FrameLog,
    Timeline,
    Where,
    normalize_types,
)

#: 默认等待上限（design D4：``expect`` / ``expect_none`` 必须显式或默认带 ``within``）。
DEFAULT_WITHIN = 5.0


class Watcher:
    """一条时间线的断言门面（``Session.watch``）。

    无状态之外的全部状态都在 ``Timeline`` 里：游标、事件、等待唤醒。
    """

    def __init__(
        self,
        timeline: Timeline,
        *,
        default_within: float = DEFAULT_WITHIN,
        frames: FrameLog | Sequence[Frame] | None = None,
        dump_path: str | None = None,
    ) -> None:
        self.timeline = timeline
        self.default_within = default_within
        self.frames = frames
        """原始帧日志（报告"原始帧尾部"用；可后置注入）。"""
        self.dump_path = dump_path
        """现场转储路径（失败报告末行；由 ``probe.dump()`` 侧填充）。"""

    # ── 查询 ──────────────────────────────────────────────────

    @property
    def cursor(self) -> int:
        return self.timeline.cursor

    def pending(self) -> list[Event]:
        """游标之后的事件（报告与自定义断言的入口）。"""
        return self.timeline.pending()

    def events(
        self,
        type: str | Iterable[str] | None = None,
        *,
        where: Where | None = None,
    ) -> list[Event]:
        """全量查询（含已消费事件），不推进游标。"""
        return self.timeline.events(type, where=where)

    # ── 正向等待 ──────────────────────────────────────────────

    async def expect(
        self,
        type: str | Iterable[str],
        *,
        where: Where | None = None,
        within: float | None = None,
    ) -> Event:
        """等待游标后首个匹配事件；命中推进游标并返回它。

        ``type`` 可以是单个类型名或一组类型名（"turn_result 或 error"这类或语义）。
        超时抛 ``ExpectationError``（含游标后的完整时间线与原始帧尾部）。
        """
        types = normalize_types(type)
        limit = self._within(within)
        expectation = Expectation(
            kind=KIND_EXPECT,
            types=types,
            within=limit,
            where=describe_where(where),
        )
        match = await self.timeline.wait_for(
            lambda event: event.matches(types, where), within=limit
        )
        if match is None:
            raise self._failure(expectation)
        self.timeline.set_cursor(match.index + 1)
        return match

    # ── 负向断言 ──────────────────────────────────────────────

    async def expect_none(
        self,
        types: str | Iterable[str],
        *,
        where: Where | None = None,
        within: float | None = None,
    ) -> None:
        """未来窗口（游标起 ``within`` 秒）内不得出现给定类型的事件。

        命中即失败（不等到窗口结束——出现就不可能撤回）；超时未出现即通过。
        游标不动：窗口里的其他事件仍可被后续断言匹配。
        """
        target = normalize_types(types)
        limit = self._within(within)
        expectation = Expectation(
            kind=KIND_EXPECT_NONE,
            types=target,
            within=limit,
            where=describe_where(where),
        )
        unexpected = await self.timeline.wait_for(
            lambda event: event.matches(target, where), within=limit
        )
        if unexpected is not None:
            raise self._failure(
                expectation,
                focus=unexpected,
                detail=(
                    f"first divergence: unexpected {unexpected.type!r} event at "
                    f"index {unexpected.index} ({format_at(unexpected.at)})"
                ),
            )

    def assert_never(
        self,
        types: str | Iterable[str],
        *,
        where: Where | None = None,
    ) -> None:
        """全时间线（含已消费事件）从未出现给定类型的事件。"""
        target = normalize_types(types)
        expectation = Expectation(
            kind=KIND_NEVER, types=target, where=describe_where(where)
        )
        for event in self.timeline.all():
            if event.matches(target, where):
                raise self._failure(
                    expectation,
                    focus=event,
                    detail=(
                        f"first divergence: {event.type!r} occurred at index "
                        f"{event.index} ({format_at(event.at)})"
                    ),
                )

    # ── 顺序断言 ──────────────────────────────────────────────

    def assert_ordered(
        self,
        types: str | Iterable[str],
        *,
        where: Where | None = None,
        since: int | None = None,
    ) -> None:
        """扫描窗口内 ``types`` 按给定顺序出现（允许夹杂其他事件）。

        窗口默认从游标开始，``since`` 可显式指定起点序号——"等整轮结束后再断言
        这一轮的事件顺序"就得用它（``expect`` 会把游标推过去）::

            start = session.watch.cursor
            await session.send("hi")
            await session.watch.expect("turn_result")
            session.watch.assert_ordered(["tool_call", "tool_call_result"], since=start)

        成功时游标推进到最后一次命中之后；失败时报告第一处分叉：期望类型、它应处
        的位置、已匹配到的前缀、以及被跳过的实际事件序列。
        """
        order = normalize_types(types)
        start = self.timeline.cursor if since is None else since
        expectation = Expectation(
            kind=KIND_ORDERED, types=order, where=describe_where(where)
        )
        events = self.timeline.all()
        position = 0
        matched: list[Event] = []
        skipped: list[Event] = []
        for index in range(max(start, 0), len(events)):
            event = events[index]
            if (
                position < len(order)
                and event.type == order[position]
                and (where is None or event.matches((order[position],), where))
            ):
                matched.append(event)
                position += 1
                if position == len(order):
                    # 游标只前进不后退（since= 回看窗口时不得把已消费事件重新暴露）
                    self.timeline.set_cursor(max(self.timeline.cursor, event.index + 1))
                    return
            elif position < len(order):
                skipped.append(event)
        raise self._failure(
            expectation,
            detail=_divergence_detail(order, position, matched, skipped),
        )

    # ── 内部 ──────────────────────────────────────────────────

    def _within(self, within: float | None) -> float:
        limit = self.default_within if within is None else within
        if limit is None or math.isinf(limit) or math.isnan(limit):
            raise ValueError(
                f"within must be a finite number of seconds, got {limit!r} "
                "(unbounded waits are not allowed)"
            )
        if limit < 0:
            raise ValueError(f"within must be >= 0, got {limit}")
        return float(limit)

    def _failure(
        self,
        expectation: Expectation,
        *,
        focus: Event | None = None,
        detail: str | None = None,
    ) -> ExpectationError:
        if detail is not None:
            expectation = replace(expectation, detail=detail)
        return ExpectationError(
            expectation,
            timeline=self.timeline,
            frames=self.frames,
            focus=focus,
            dump_path=self.dump_path,
        )


def _divergence_detail(
    order: tuple[str, ...],
    position: int,
    matched: list[Event],
    skipped: list[Event],
) -> str:
    """顺序断言的第一处分叉（期望 vs 实际）。"""
    expected = order[position]
    lines = [
        f"first divergence: position {position} expected {expected!r}, "
        f"but no matching event followed"
    ]
    if matched:
        done = ", ".join(
            f"{event.type!r} (index {event.index}, {format_at(event.at)})"
            for event in matched
        )
        lines.append(f"  matched so far: {done}")
    else:
        lines.append("  matched so far: (nothing)")
    if skipped:
        seen = ", ".join(f"{event.type}@{event.index}" for event in skipped[:10])
        if len(skipped) > 10:
            seen += f", … (+{len(skipped) - 10} more)"
        lines.append(f"  skipped instead: [{seen}]")
    else:
        lines.append("  skipped instead: (no further event at all)")
    return "\n".join(lines)
