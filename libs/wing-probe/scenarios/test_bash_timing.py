"""Bash process timing on live events and late subscriptions."""

import asyncio

import pytest

from wing_probe import Driver, Probe, ToolCall, Turn

MODEL = "probe/bash-timing"
CONFIRM_MODEL = "probe/bash-timing-confirm"
CALL_IDS = ("call-first", "call-second")
TIMING = "tool_execution_timing"


async def late_snapshot(probe: Probe, session_id: str) -> dict:
    other = await Driver.connect(probe.env)
    try:
        joined = await other.attach(session_id)
        return (await joined.watch.expect("sync_session", within=5)).data
    finally:
        await other.close()


def _gated_command(call_id: str) -> str:
    return (
        "i=0; "
        f'while [ ! -f {call_id} ] && [ "$i" -lt 200 ]; '
        "do i=$((i+1)); sleep 0.05; done; "
        f"[ -f {call_id} ] && sleep 0.1"
    )


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.asyncio
async def test_late_join_during_parallel_bash_calls(probe: Probe) -> None:
    probe.register(
        MODEL,
        Turn.of(
            first_delay=1.0,
            tool_calls=[
                ToolCall("Bash", {"command": _gated_command(call_id)}, id=call_id)
                for call_id in CALL_IDS
            ],
        ),
    )
    session = await probe.session(model=MODEL, yolo=True)
    first, second = (probe.files.resolve(call_id) for call_id in CALL_IDS)
    try:
        await session.send("Start both commands")
        turn = await session.watch.expect("turn_started", within=5)
        starts = [await session.watch.expect(TIMING, within=5) for _ in CALL_IDS]
        assert {event.data["tool_call_id"] for event in starts} == set(CALL_IDS)
        assert all(event.at - turn.at >= 0.7 for event in starts)

        sync = await late_snapshot(probe, session.session_id)
        assert sync["status"] == "working"
        assert {call["id"] for call in sync["uncommitted"]["tool_calls"]} == set(
            CALL_IDS
        )
        running = sync["tool_execution_timings"]
        assert set(running) == set(CALL_IDS), running
        for event in starts:
            live = event.data["timing"]
            snapshot = running[event.data["tool_call_id"]]
            assert live["running"] and snapshot["running"], (live, snapshot)
            assert snapshot["timeout_secs"] == live["timeout_secs"] == 30
            assert snapshot["elapsed_ms"] >= live["elapsed_ms"]

        first.touch()
        finished = await session.watch.expect(
            TIMING, where={"tool_call_id": CALL_IDS[0]}, within=5
        )
        timing = finished.data["timing"]
        assert not timing["running"] and timing["timeout_secs"] == 30, timing
        assert 100 <= timing["elapsed_ms"] < (finished.at - turn.at) * 1000 - 500
        result = await session.watch.expect("tool_call_result", within=5)
        assert (
            result.data["tool_call_id"] == CALL_IDS[0] and result.data["tool_success"]
        )
        assert result.data["duration_ms"] >= timing["elapsed_ms"]
        remaining = (await late_snapshot(probe, session.session_id))[
            "tool_execution_timings"
        ]
        assert set(remaining) == {CALL_IDS[1]}, remaining
        assert (
            remaining[CALL_IDS[1]]["elapsed_ms"] >= running[CALL_IDS[1]]["elapsed_ms"]
        )

        assert (await session.interrupt())["ok"] is True
        interrupted_result = await session.watch.expect("tool_call_result", within=5)
        assert interrupted_result.data["tool_call_id"] == CALL_IDS[1]
        assert not interrupted_result.data["tool_success"]
        assert "duration_ms" not in interrupted_result.data
        await session.watch.expect("interrupted", within=5)
        assert [
            event.data["timing"]["running"]
            for event in session.watch.events(
                type=TIMING, where={"tool_call_id": CALL_IDS[1]}
            )
        ] == [True]
    finally:
        first.touch()
        second.touch()

    idle = await late_snapshot(probe, session.session_id)
    assert idle["status"] == "idle" and not idle["tool_execution_timings"], idle
    history = probe.history(session)
    history.assert_tool_pairing()
    history.assert_no_transient_records()


@pytest.mark.probe_env(models=[CONFIRM_MODEL])
@pytest.mark.asyncio
@pytest.mark.parametrize("answer", ["n", "y"])
async def test_bash_timing_begins_only_after_confirmation(
    probe: Probe, answer: str
) -> None:
    probe.register(
        CONFIRM_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall("Bash", {"command": "printf ran > marker", "timeout": 1})
            ]
        ),
        Turn.of(text="handled"),
    )
    session = await probe.session(model=CONFIRM_MODEL, yolo=False)
    await session.send("Run the command")
    ask = await session.watch.expect("ask", within=5)
    call_id = ask.data["tool_call_id"]
    waiting = await late_snapshot(probe, session.session_id)
    assert waiting["status"] == "waiting" and not waiting["tool_execution_timings"], (
        waiting
    )
    session.watch.assert_never(TIMING)
    probe.files.assert_missing("marker")

    if answer == "y":
        # Keep the approval pending longer than the subprocess's timeout budget.
        await asyncio.sleep(1.1)
    await session.answer(ask, answer)
    result = await session.watch.expect("tool_call_result", within=5)
    assert result.data["tool_call_id"] == call_id
    timings = session.watch.events(type=TIMING)
    if answer == "n":
        assert not timings, timings
        assert not result.data["tool_success"]
        probe.files.assert_missing("marker")
    else:
        assert result.data["tool_success"] and result.data["duration_ms"] >= 1100, (
            result.data
        )
        probe.files.assert_content("marker", equals="ran")
        assert [event.data["tool_call_id"] for event in timings] == [call_id, call_id]
        started, finished = (event.data["timing"] for event in timings)
        assert started["running"] and not finished["running"], (started, finished)
        assert started["timeout_secs"] == finished["timeout_secs"] == 1
        assert 0 <= finished["elapsed_ms"] < 1000, finished
    turn_result = await session.watch.expect("turn_result", within=5)
    assert turn_result.data["subtype"] == "success", turn_result.data


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.asyncio
async def test_bash_timeout_reports_measured_process_duration(probe: Probe) -> None:
    probe.register(
        MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "sleep 3", "timeout": 1})]),
        Turn.of(text="handled"),
    )
    session = await probe.session(model=MODEL, yolo=True)
    await session.send("Run the command")
    started = await session.watch.expect(TIMING, within=5)
    finished = await session.watch.expect(TIMING, within=5)
    result = await session.watch.expect("tool_call_result", within=5)
    assert (
        started.data["tool_call_id"]
        == finished.data["tool_call_id"]
        == result.data["tool_call_id"]
    )
    assert started.data["timing"]["running"]
    timing = finished.data["timing"]
    assert not timing["running"] and timing["timeout_secs"] == 1, timing
    assert 900 <= timing["elapsed_ms"] <= result.data["duration_ms"], result.data
    assert not result.data["tool_success"] and "timed out" in result.data["tool_result"]
    turn_result = await session.watch.expect("turn_result", within=5)
    assert turn_result.data["subtype"] == "success", turn_result.data
