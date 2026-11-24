"""Bash execution facts for clients that subscribe after the tools start."""

from datetime import datetime

import pytest

from wing_probe import Driver, Probe, ToolCall, Turn

MODEL = "probe/bash-timing"
CALL_IDS = ("call-first", "call-second")


async def late_snapshot(probe: Probe, session_id: str) -> dict:
    other = await Driver.connect(probe.env)
    try:
        joined = await other.attach(session_id)
        return (await joined.watch.expect("sync_session", within=5)).data
    finally:
        await other.close()


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.asyncio
async def test_late_join_during_parallel_bash_calls(probe: Probe) -> None:
    probe.register(
        MODEL,
        Turn.of(
            first_delay=0.5,
            tool_calls=[
                ToolCall(
                    "Bash",
                    {
                        "command": f"while [ ! -f {call_id} ]; do sleep 0.05; done; sleep 0.1"
                    },
                    id=call_id,
                )
                for call_id in CALL_IDS
            ],
        ),
        Turn.of(text="done"),
    )
    session = await probe.session(model=MODEL, yolo=True)
    await session.send("Start both commands")
    turn = await session.watch.expect("turn_started", within=5)
    starts = [await session.watch.expect("tool_call", within=5) for _ in CALL_IDS]
    assert {event.data["tool_call_id"] for event in starts} == set(CALL_IDS)
    assert all(event.at - turn.at >= 0.3 for event in starts)

    first, second = (probe.files.resolve(call_id) for call_id in CALL_IDS)
    try:
        sync = await late_snapshot(probe, session.session_id)
        assert sync["status"] == "working"
        assert {call["id"] for call in sync["uncommitted"]["tool_calls"]} == set(
            CALL_IDS
        )
        assert "tool_started_at" in sync, sync
        running = sync["tool_started_at"]
        assert set(running) == set(CALL_IDS), running
        for event in starts:
            call_id = event.data["tool_call_id"]
            started_at = event.data["started_at"]
            assert running[call_id] == started_at
            assert datetime.fromisoformat(started_at) > datetime.fromisoformat(
                sync["turn_started_at"]
            )

        first.touch()
        result = await session.watch.expect("tool_call_result", within=5)
        assert result.data["tool_call_id"] == CALL_IDS[0], result.data
        assert result.data["tool_success"] and result.data["duration_ms"] >= 100, (
            result.data
        )
        assert (await late_snapshot(probe, session.session_id))["tool_started_at"] == {
            CALL_IDS[1]: running[CALL_IDS[1]],
        }
    finally:
        first.touch()
        second.touch()

    turn_result = await session.watch.expect("turn_result", within=5)
    assert turn_result.data["subtype"] == "success", turn_result.data
    await session.watch.expect("done", within=5)
    idle = await late_snapshot(probe, session.session_id)
    assert idle["status"] == "idle" and not idle["tool_started_at"], idle
    assert not any(
        event["type"] in {"tool_call", "tool_call_result"}
        for event in probe.history(session).events()
    )
