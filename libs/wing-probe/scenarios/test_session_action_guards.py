"""Directed feedback and rewind admission through the public gateway."""

import asyncio
from contextlib import AsyncExitStack
from dataclasses import replace

import pytest

from wing_probe import (
    Driver,
    DriverHttpError,
    Probe,
    Session,
    ToolCall,
    Turn,
    assert_rewind_transition,
)
from wing_probe.toolhost import ToolHost

MODEL = "probe/session-action-guards"
ASK = ToolCall(
    "AskUserQuestion",
    {
        "questions": [
            {
                "id": "color",
                "header": "Color",
                "question": "Which color?",
                "options": [{"label": "red"}],
            }
        ]
    },
)
pytestmark = [pytest.mark.asyncio, pytest.mark.probe_env(models=[MODEL])]


async def _reject_answer(
    session: Session, transport: str, call_id: str, label: str
) -> str:
    request_id = f"rejected-{label}"
    content = f"bad answer ({label})"
    if transport == "http":
        with pytest.raises(DriverHttpError) as rejected:
            outcome = await session.driver.http.request(
                "POST",
                "/api/session/send",
                body={
                    "session_id": session.session_id,
                    "request_id": request_id,
                    "content": content,
                    "tool_call_id": call_id,
                },
            )
            call = session.driver.http.last_call(path="/api/session/send")
            assert call is not None
            pytest.fail(f"directed answer returned HTTP {call.status} with {outcome!r}")
        assert rejected.value.status == 400, rejected.value
        response = rejected.value.call.response
        assert isinstance(response, dict), response
        message = response["detail"]
    else:
        await session.driver.websocket.send_request(
            session.session_id, content, tool_call_id=call_id, request_id=request_id
        )
        event = await session.watch.expect(
            ["error", "user_message_queued"],
            where=lambda e: (
                request_id
                in (e.data.get("request_id"), e.data.get("origin_request_id"))
            ),
        )
        assert event.type == "error", event.data
        assert event.data["session_id"] == session.session_id, event.data
        assert event.data["request_id"] == request_id, event.data
        assert event.data["status_code"] == 400, event.data
        assert event.data["error_code"] == "feedback_waiter_inactive", event.data
        message = event.data["message"]
    assert message == "no active feedback waiter for this tool_call_id", str(message)[
        :100
    ]
    return request_id


@pytest.mark.parametrize("transport", ["http", "ws"])
async def test_directed_answers_never_become_user_input(
    probe: Probe, transport: str
) -> None:
    probe.register(
        MODEL,
        Turn.of(tool_calls=[ASK]),
        Turn.of(text="answered"),
        Turn.of(tool_calls=[ASK]),
        Turn.of(text="ordinary turn"),
    )
    session = await probe.session(model=MODEL)
    await session.send("start ask")
    ask = await session.watch.expect("ask")
    call_id = ask.data["tool_call_id"]

    unknown_id = await _reject_answer(session, transport, "unknown-call", "unknown")
    assert (await session.info())["status"] == "waiting"
    if transport == "http":
        answer = await session.driver.http.send_message(
            session.session_id, "red", call_id
        )
        assert answer["ok"] is True, answer
    else:
        await session.answer(call_id, "red")
    result = await session.watch.expect(
        "tool_call_result", where={"tool_call_id": call_id}
    )
    assert result.data["tool_result"] == "red", result.data
    assert (await session.watch.expect("turn_result")).data["result"] == "answered"
    await session.watch.expect("done")
    duplicate_id = await _reject_answer(session, transport, call_id, "duplicate")
    assert (await session.info())["status"] == "idle"

    await session.send("ask before interrupt")
    expired = await session.watch.expect("ask")
    expired_id = expired.data["tool_call_id"]
    assert expired_id != call_id
    await session.interrupt()
    late_id = await _reject_answer(session, transport, expired_id, "expired")
    assert (await session.info())["status"] == "idle"

    result = await session.chat("ordinary input")
    assert result.data["result"] == "ordinary turn", result.data
    await session.watch.expect("done")
    for request_id in (unknown_id, duplicate_id, late_id):
        session.watch.assert_never("delivered", where={"request_id": request_id})
    assert probe.requests.count(MODEL) == 4, probe.requests.summary()
    assert [
        m["content"] for m in session.history.messages() if m["role"] == "user"
    ] == ["start ask", "ask before interrupt", "ordinary input"]
    assert [e.data["content"] for e in session.watch.events("user_message_queued")] == [
        "start ask",
        "ask before interrupt",
        "ordinary input",
    ]


async def test_rejected_ws_answer_preserves_other_waiters(probe: Probe) -> None:
    probe.register(
        MODEL,
        Turn.of(tool_calls=[replace(ASK, id="ask-a"), replace(ASK, id="ask-b")]),
        Turn.of(text="both answered"),
    )
    session = await probe.session(model=MODEL)
    controller = await Driver.connect(probe.env)
    try:
        other = await controller.attach(session.session_id)
        await session.send("ask two questions")
        for watcher in (session.watch, other.watch):
            asks = [await watcher.expect("ask") for _ in range(2)]
            assert {ask.data["tool_call_id"] for ask in asks} == {"ask-a", "ask-b"}

        await session.answer("ask-a", "first answer")
        await session.watch.expect("tool_call_result", where={"tool_call_id": "ask-a"})
        duplicate_id = await _reject_answer(other, "ws", "ask-a", "competing-answer")
        # This ID fits in an incoming frame. Echoing repr(id) in JSON would exceed 16 MiB.
        long_id = await _reject_answer(other, "ws", "\u0085" * 3_400_000, "long-id")
        assert (await other.info())["status"] == "waiting"
        assert probe.requests.count(MODEL) == 1, probe.requests.summary()

        await other.answer("ask-b", "second answer")
        result = await other.watch.expect(
            "tool_call_result", where={"tool_call_id": "ask-b"}
        )
        assert result.data["tool_result"] == "second answer", result.data
        assert (await other.watch.expect("turn_result")).data[
            "result"
        ] == "both answered"
        await other.watch.expect("done")
        await session.watch.expect("done")
        for request_id in (duplicate_id, long_id):
            session.watch.assert_never("error", where={"request_id": request_id})
            other.watch.assert_never("delivered", where={"request_id": request_id})
        assert probe.requests.count(MODEL) == 2, probe.requests.summary()
        assert [
            m["content"] for m in session.history.messages() if m["role"] == "user"
        ] == ["ask two questions"]
    finally:
        await controller.close()


@pytest.mark.parametrize("phase", ["ask", "tool"])
async def test_rewind_refused_during_a_turn(probe: Probe, phase: str) -> None:
    release = asyncio.Event()

    async def held_tool() -> str:
        await release.wait()
        return "tool finished"

    async with AsyncExitStack() as cleanup:
        host = None
        tools = ["AskUserQuestion"]
        tool_call = ASK
        if phase == "tool":
            host = ToolHost(
                "rewind-probe", probe.env.gateway_url, started_at=probe.env.started_at
            )
            cleanup.push_async_callback(host.close)
            cleanup.callback(release.set)
            host.add_tool("HeldTool", held_tool)
            await host.start()
            tools = ["rewind-probe.HeldTool"]
            tool_call = ToolCall("HeldTool", {})
        controller = await Driver.connect(probe.env)
        cleanup.push_async_callback(controller.close)
        probe.register(
            MODEL,
            Turn.of(text="seed reply"),
            Turn.of(tool_calls=[tool_call]),
            Turn.of(text="turn finished"),
        )
        session = await probe.session(model=MODEL, tools=tools, yolo=True)
        await session.chat("seed")
        await session.watch.expect("done")
        target = session.history.messages()[0]["uuid"]
        await session.send("hold this turn")
        if host is None:
            ask = await session.watch.expect("ask")
        else:
            await host.wait_for_call("HeldTool", timeout=10)

        other = await controller.attach(session.session_id)
        assert (await other.info())["status"] == (
            "waiting" if phase == "ask" else "working"
        )
        before = session.history
        current = await other.rewind("current")
        assert current == {"ok": True, "draft": None}, current
        with pytest.raises(DriverHttpError) as refused:
            outcome = await other.rewind(target)
            pytest.fail(
                f"rewind returned {outcome!r} and tip became {session.history.tip_uuid}"
            )
        assert refused.value.status == 409, refused.value
        response = refused.value.call.response
        assert isinstance(response, dict), response
        assert "interrupt" in response["detail"], refused.value
        after = session.history
        assert after.records == before.records and after.tip_uuid == before.tip_uuid

        if host is None:
            await session.answer(ask, "red")
        else:
            release.set()
        assert (await session.watch.expect("turn_result")).data[
            "result"
        ] == "turn finished"
        await session.watch.expect("done")
        assert (await other.info())["status"] == "idle"
        before = session.history
        assert (await other.rewind(target))["draft"] == "seed"
        assert_rewind_transition(before, session.history, target)
