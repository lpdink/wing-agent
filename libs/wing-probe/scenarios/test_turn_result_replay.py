from __future__ import annotations

import pytest
from _session_helpers import evict

from wing_probe import Probe, Turn


@pytest.mark.asyncio
async def test_turn_results_match_live_and_cold_replay(probe: Probe) -> None:
    model = "probe/default"
    probe.register(model, Turn.of(text="First reply"), Turn.of(text="Second reply"))
    session = await probe.session(model=model)

    first = await session.chat("First prompt")
    second = await session.chat("Second prompt")
    expected = [
        ("First reply", {"is_error": False, "duration_ms": first.data["duration_ms"]}),
        (
            "Second reply",
            {"is_error": False, "duration_ms": second.data["duration_ms"]},
        ),
    ]
    initial = (await session.get())["messages"]
    assert [
        (message["content"], message["turn_result"])
        for message in initial
        if "turn_result" in message
    ] == expected

    await evict(probe, session.session_id)
    await probe.resume(session.session_id)
    driver = probe.driver_required
    await driver.http.subscribe(session.session_id, driver.client_id)
    snapshot = await session.watch.expect("sync_session")
    assert snapshot.data["messages"] == initial
    assert (await session.get())["messages"] == initial

    await session.rewind(initial[-1]["uuid"])
    snapshot = await session.watch.expect("sync_session")
    assert [
        (message["content"], message["turn_result"])
        for message in snapshot.data["messages"]
        if "turn_result" in message
    ] == expected[:1]
