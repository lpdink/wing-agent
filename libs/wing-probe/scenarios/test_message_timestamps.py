from __future__ import annotations

import json
from datetime import datetime

import pytest
from _session_helpers import evict

from wing_probe import Probe, ToolCall, Turn


MODEL = "probe/message-timestamps"


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.asyncio
async def test_message_timestamps_match_live_history_and_resume(probe: Probe) -> None:
    probe.register(
        MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "printf ok"})]),
        Turn.of(text="Done"),
    )
    session = await probe.session(model=MODEL, yolo=True)
    await session.chat("Check the timestamp")

    history = await session.get()
    user, tool_request, tool_result, assistant = history["messages"]
    assert [message["role"] for message in history["messages"]] == [
        "user",
        "assistant",
        "tool",
        "assistant",
    ]
    stored = {row["uuid"]: row["ts"] for row in probe.history(session).messages()}
    assert len(stored) == 4
    for message in history["messages"]:
        assert message["ts"] == stored[message["uuid"]]
        datetime.fromisoformat(message["ts"])
    (committed,) = session.watch.events("user_message_committed")
    first, last = session.watch.events("assistant_turn")
    assert (committed.data["message_uuid"], committed.data["ts"]) == (
        user["uuid"],
        user["ts"],
    )
    assert [first.data["ts"], last.data["ts"]] == [tool_request["ts"], assistant["ts"]]

    await evict(probe, session.session_id)
    # Simulate a legacy record without a timestamp while the session is unloaded.
    path = probe.env.sessions_path / session.session_id / "history.jsonl"
    records = [json.loads(line) for line in path.read_text().splitlines()]
    for record in records:
        if record["uuid"] == tool_result["uuid"]:
            del record["ts"]
    path.write_text("".join(json.dumps(record) + "\n" for record in records))

    await probe.resume(session.session_id)
    driver = probe.driver_required
    await driver.http.subscribe(session.session_id, driver.client_id)
    sync = await session.watch.expect("sync_session")
    del tool_result["ts"]
    assert sync.data["messages"] == history["messages"]
    assert (await session.get())["messages"] == history["messages"]
