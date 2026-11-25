"""Tool timing through the executor, tool context, and live event sink."""

from __future__ import annotations

import asyncio
from collections.abc import Iterator
from types import SimpleNamespace

import pytest

from _support import make_tool
from wing.agent import tool_executor
from wing.agent.event_sink import AgentEventSink
from wing.agent.tool_context import measure_tool_execution
from wing.agent.tool_executor import (
    INTERRUPTED_RESULT,
    InterruptedToolResults,
    ToolExecutor,
)
from wing.event import (
    ToolCallResultEvent,
    ToolExecutionStartedEvent,
    ToolExecutionTimingEvent,
    WingEvent,
)
from wing.event_bus import event_bus
from wing.schema import ToolCall

pytestmark = pytest.mark.asyncio
CALL = ToolCall(id="timed-call", name="TimedTool", arguments={})


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> SimpleNamespace:
    clock = SimpleNamespace(now=100.0)
    monkeypatch.setattr(
        tool_executor, "time", SimpleNamespace(monotonic=lambda: clock.now)
    )
    return clock


@pytest.fixture
def observed_executor() -> Iterator[tuple[ToolExecutor, list[WingEvent]]]:
    live: list[WingEvent] = []
    persisted: list[WingEvent] = []
    sink = AgentEventSink(session_id="timing-session", append_event=persisted.append)
    event_bus.subscribe(live.append)
    try:
        yield ToolExecutor(sink), live
    finally:
        event_bus.unsubscribe(live.append)
    assert persisted == []


def _timings(live: list[WingEvent]) -> list[tuple[bool, int, float | None]]:
    timings = [event for event in live if isinstance(event, ToolExecutionTimingEvent)]
    assert all(
        event.session_id == "timing-session" and event.tool_call_id == CALL.id
        for event in timings
    )
    return [
        (event.timing.running, event.timing.elapsed_ms, event.timing.timeout_secs)
        for event in timings
    ]


async def test_tool_execution_snapshots_advance_then_freeze(clock, observed_executor):
    executor, live = observed_executor
    snapshots = []

    async def tool() -> str:
        with measure_tool_execution(10):
            clock.now = 101.25
            snapshots.append(executor.tool_execution_timings())
            clock.now = 102.5
        clock.now = 107.0
        snapshots.append(executor.tool_execution_timings())
        return "done"

    executor.set_tools({CALL.name: make_tool(CALL.name, tool)})
    assert executor.tool_execution_timings() == {}
    (message,) = await executor.execute([CALL], "test-model")
    assert message.content == "done"

    assert [event.type for event in live] == [
        "tool_call",
        "tool_execution_started",
        "tool_execution_timing",
        "tool_execution_timing",
        "tool_call_result",
        "tool_result_turn",
    ]
    started = next(
        event for event in live if isinstance(event, ToolExecutionStartedEvent)
    )
    assert started.tool_call_id == CALL.id and started.tool_args == CALL.arguments
    assert _timings(live) == [(True, 0, 10), (False, 2500, 10)]
    running, finished = snapshots
    assert set(running) == set(finished) == {CALL.id}
    assert running[CALL.id].running and running[CALL.id].elapsed_ms == 1250
    assert not finished[CALL.id].running and finished[CALL.id].elapsed_ms == 2500
    assert running[CALL.id].timeout_secs == finished[CALL.id].timeout_secs == 10
    result = next(event for event in live if isinstance(event, ToolCallResultEvent))
    assert result.tool_success and result.duration_ms == 7000
    assert executor.tool_execution_timings() == {}


@pytest.mark.parametrize(
    ("error", "expected"),
    [
        (TimeoutError("budget expired"), [(True, 0, 2), (False, 2000, 2)]),
        (RuntimeError("process wait failed"), [(True, 0, 2)]),
    ],
)
async def test_tool_execution_timing_on_error(
    clock, observed_executor, error, expected
):
    executor, live = observed_executor

    async def tool() -> str:
        with measure_tool_execution(2):
            clock.now = 102.0
            raise error

    executor.set_tools({CALL.name: make_tool(CALL.name, tool)})
    (message,) = await executor.execute([CALL], "test-model")
    assert message.content and message.content.endswith(str(error))
    assert _timings(live) == expected
    result = next(event for event in live if isinstance(event, ToolCallResultEvent))
    assert not result.tool_success
    assert executor.tool_execution_timings() == {}


async def test_cancelled_tool_drops_timing_without_a_completion(
    clock, observed_executor
):
    executor, live = observed_executor
    entered = asyncio.Event()

    async def tool() -> str:
        with measure_tool_execution(10):
            entered.set()
            await asyncio.get_running_loop().create_future()
        return "done"

    executor.set_tools({CALL.name: make_tool(CALL.name, tool)})
    task = asyncio.create_task(executor.execute([CALL], "test-model"))
    try:
        await asyncio.wait_for(entered.wait(), timeout=5)
        clock.now = 101.0
        running = executor.tool_execution_timings()[CALL.id]
        assert running.running and running.elapsed_ms == 1000
    finally:
        task.cancel()
        with pytest.raises(InterruptedToolResults) as caught:
            await task

    assert [message.content for message in caught.value.results] == [INTERRUPTED_RESULT]
    assert _timings(live) == [(True, 0, 10)]
    result = next(event for event in live if isinstance(event, ToolCallResultEvent))
    assert not result.tool_success and result.duration_ms is None
    assert executor.tool_execution_timings() == {}


async def test_timing_inherited_from_a_finished_batch_is_ignored(observed_executor):
    executor, live = observed_executor
    release = asyncio.Event()
    children: list[asyncio.Task[None]] = []

    async def delayed_measurement() -> None:
        await release.wait()
        with measure_tool_execution(10):
            assert executor.tool_execution_timings() == {}

    async def tool() -> str:
        children.append(asyncio.create_task(delayed_measurement()))
        return "done"

    executor.set_tools({CALL.name: make_tool(CALL.name, tool)})
    try:
        (message,) = await executor.execute([CALL], "test-model")
        assert message.content == "done"
    finally:
        release.set()
        await asyncio.gather(*children)

    assert _timings(live) == []
    assert executor.tool_execution_timings() == {}
