"""Ask settlement comes from accepted feedback and survives result hooks and replay."""

from __future__ import annotations

import pytest

from wing_probe import Driver, DriverHttpError, Probe, Session, ToolCall, ToolHost, Turn

MODEL = "probe/ask-outcome"
ASK = "AskUserQuestion"
CANCEL_TOKEN = "__wing_ask_cancelled__"
CANCEL_TEXT = "User cancelled the questions. Proceed with your best judgment."
REWRITTEN_TEXT = "The user chose to skip these questions (test hook)."

pytestmark = pytest.mark.asyncio


def _ask(call_id: str) -> ToolCall:
    return ToolCall(
        ASK,
        {
            "questions": [
                {"id": "question", "header": "Question", "question": "Continue?"}
            ]
        },
        id=call_id,
    )


async def _feedback(driver: Driver, session: Session, call_id: str, text: str) -> dict:
    return await driver.http.request(
        "POST",
        "/api/session/send",
        body={
            "session_id": session.session_id,
            "tool_call_id": call_id,
            "content": text,
        },
    )


def _assert_result(row: dict, event: dict) -> None:
    for key in ("message_id", "tool_call_id", "tool_status", "ask_outcome"):
        assert row[key] == event[key]
    assert row["content"] == event["tool_result"]


@pytest.mark.probe_env(models=[MODEL], hooks=["hooks/*.py"])
async def test_outcomes_survive_hook_text_and_parallel_stale_feedback(
    probe: Probe,
) -> None:
    hooks = probe.env.root / "hooks"
    hooks.mkdir()
    (hooks / "ask.py").write_text(
        f"""from wing.hooks import hooks

@hooks.on("after_tool_call")
def rewrite(content, *, tool_name, tool_call_id, **kwargs):
    if tool_name == {ASK!r} and tool_call_id == "call_cancel" and content == {CANCEL_TEXT!r}:
        return {REWRITTEN_TEXT!r}
""",
        encoding="utf-8",
    )
    assert (await probe.driver_required.http.reload())["ok"]
    cancel, literal, pending = "call_cancel", "call_literal", "call_pending"
    probe.register(
        MODEL,
        Turn.of(tool_calls=[_ask(call_id) for call_id in (cancel, literal, pending)]),
        Turn.of(text="continued"),
    )
    session = await probe.session(model=MODEL, tools=[ASK])
    await session.watch.expect("sync_session")
    await session.send("collect feedback")
    calls = {(await session.watch.expect("ask")).data["tool_call_id"] for _ in range(3)}
    assert calls == {cancel, literal, pending}

    other = await Driver.connect(probe.env)
    try:
        await session.answer(cancel, CANCEL_TOKEN)
        cancelled = (
            await session.watch.expect(
                "tool_call_result", where={"tool_call_id": cancel}
            )
        ).data
        assert cancelled["ask_outcome"] == "cancelled"
        assert cancelled["tool_result"] == REWRITTEN_TEXT
        assert (
            cancelled["tool_status"] == "success" and cancelled["tool_success"] is True
        )

        with pytest.raises(DriverHttpError) as rejected:
            await _feedback(other, session, cancel, CANCEL_TEXT)
        assert rejected.value.status == 400
        assert (
            rejected.value.call.response["detail"]
            == "no active feedback waiter for this tool_call_id"
        )
        assert (await _feedback(other, session, literal, CANCEL_TEXT))["ok"]
        answered = (
            await session.watch.expect(
                "tool_call_result", where={"tool_call_id": literal}
            )
        ).data
        assert answered["ask_outcome"] == "answered"
        assert answered["tool_result"] == CANCEL_TEXT
        assert answered["tool_status"] == "success" and answered["tool_success"] is True

        await session.answer(literal, CANCEL_TOKEN)
        rejected_live = await session.watch.expect(
            "error", where={"error_code": "feedback_waiter_inactive"}
        )
        assert rejected_live.data["status_code"] == 400
        joined = await other.attach(session.session_id)
        sync = (await joined.watch.expect("sync_session")).data
        http = await session.get()
        assert sync["status"] == http["status"] == "waiting"
        assert [
            event["tool_call_id"] for event in sync["events"] if event["type"] == "ask"
        ] == [pending]
        assert sync["uncommitted_tool_results"] == http["uncommitted_tool_results"]
        uncommitted = {
            row["tool_call_id"]: row for row in sync["uncommitted_tool_results"]
        }
        assert set(uncommitted) == {cancel, literal}
        _assert_result(uncommitted[cancel], cancelled)
        _assert_result(uncommitted[literal], answered)

        assert (await _feedback(other, session, pending, "Question: continue"))["ok"]
        final_answer = (
            await joined.watch.expect(
                "tool_call_result", where={"tool_call_id": pending}
            )
        ).data
        assert final_answer["ask_outcome"] == "answered"
        await joined.watch.expect("done")
        await other.http.subscribe(session.session_id, other.client_id)
        replay = (await joined.watch.expect("sync_session")).data
        assert replay["messages"] == (await session.get())["messages"]
        assert replay["uncommitted_tool_results"] == []
        messages = {
            row["tool_call_id"]: row
            for row in replay["messages"]
            if row["role"] == "tool"
        }
        disk = {
            row["tool_call_id"]: row
            for row in session.history.messages()
            if row["role"] == "tool"
        }
        assert set(messages) == set(disk) == calls
        for event in (cancelled, answered, final_answer):
            _assert_result(messages[event["tool_call_id"]], event)
            _assert_result(disk[event["tool_call_id"]], event)
        request = probe.context(MODEL, index=1)
        request.assert_tool_pairing()
        assert request.texts("tool") == [
            REWRITTEN_TEXT,
            CANCEL_TEXT,
            "Question: continue",
        ]
    finally:
        await other.close()


@pytest.mark.probe_env(models=[MODEL])
async def test_remote_same_name_has_no_ask_outcome(probe: Probe) -> None:
    host = ToolHost(
        "ask-outcome-host", probe.env.gateway_url, started_at=probe.env.started_at
    )

    def same_text() -> str:
        return CANCEL_TEXT

    host.add_tool(ASK, same_text, params=[])
    try:
        await host.start()
        probe.register(
            MODEL,
            Turn.of(tool_calls=[ToolCall(ASK, {}, id="call_remote")]),
            Turn.of(text="continued"),
        )
        session = await probe.session(model=MODEL, tools=[f"ask-outcome-host.{ASK}"])
        await session.send("run the remote tool")
        result = (
            await session.watch.expect(
                "tool_call_result", where={"tool_call_id": "call_remote"}
            )
        ).data
        assert result["tool_name"] == ASK
        assert result["tool_result"] == CANCEL_TEXT
        assert result["tool_success"] is True
        assert "ask_outcome" not in result
        await session.watch.expect("done")
        session.watch.assert_never("ask")
        (tool,) = [
            row for row in (await session.get())["messages"] if row["role"] == "tool"
        ]
        assert "ask_outcome" not in tool
        assert tool["content"] == CANCEL_TEXT
        (disk_tool,) = [
            row for row in session.history.messages() if row["role"] == "tool"
        ]
        assert "ask_outcome" not in disk_tool
    finally:
        await host.close()
