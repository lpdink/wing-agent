"""User-input sync and steer through the gateway and both provider protocols."""

from __future__ import annotations

import asyncio
import base64
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from pathlib import Path

import pytest
import yaml
from wing_probe import (
    Driver,
    DriverHttpError,
    Probe,
    Session,
    ToolCall,
    ToolHost,
    Turn,
    assert_rewind_transition,
)

MODEL = "probe/steer-sync"
VISION_MODEL = {"name": MODEL, "capabilities": {"vision": True}}
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
)
SECOND_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNg+M8AAAICAQB7CYF4AAAAAElFTkSuQmCC"
)
TOOL_NAME = "SteerEcho"
CALL_IDS = ["call_first", "call_second"]
TOOL_TEXTS = ["first result\n", "second result\n"]
HOOK_SOURCE = """
import asyncio
from pathlib import Path

from wing.hooks import hooks

@hooks.on("before_user_message")
async def wait_for_release(content):
    if content not in ("same\\nsame", "hold", "hold swap"):
        return None
    Path("hook-entered").touch()
    try:
        while not Path("hook-release").exists():
            await asyncio.sleep(0.02)
    except asyncio.CancelledError:
        if content == "hold swap":
            Path("hook-cancelled").touch()
            while not Path("hook-release").exists():
                await asyncio.sleep(0.02)
        raise
    return f"hooked: {content}"
"""

pytestmark = [pytest.mark.asyncio, pytest.mark.timeout(45)]


@asynccontextmanager
async def _subscribe(probe: Probe, session: Session) -> AsyncIterator[Session]:
    driver = await Driver.connect(probe.env)
    try:
        yield await driver.attach(session.session_id)
    finally:
        await driver.close()


async def _install_hook(probe: Probe) -> None:
    hooks = probe.env.root / "hooks"
    hooks.mkdir()
    (hooks / "input.py").write_text(HOOK_SOURCE, encoding="utf-8")
    result = await probe.driver_required.http.request("POST", "/api/system/reload")
    assert result["ok"], result


async def _wait_for_file(path: Path) -> None:
    async with asyncio.timeout(8):
        while not path.exists():
            await asyncio.sleep(0.02)


@pytest.mark.parametrize(
    "protocol",
    [
        pytest.param(
            protocol,
            marks=pytest.mark.probe_env(
                models=[VISION_MODEL],
                provider_extra={"protocol": protocol},
                hooks=["hooks/*.py"],
            ),
        )
        for protocol in ("openai", "anthropic")
    ],
)
async def test_steer_syncs_batched_input_after_every_tool_result(
    probe: Probe, protocol: str
) -> None:
    await _install_hook(probe)
    release = asyncio.Event()
    host = ToolHost(
        "steer-probe", probe.env.gateway_url, started_at=probe.env.started_at
    )

    async def echo(text: str) -> str:
        await release.wait()
        return text

    host.add_tool(
        TOOL_NAME,
        echo,
        params=[{"name": "text", "type": "string", "description": "Text to return"}],
    )
    await host.start()
    try:
        probe.register(
            MODEL,
            Turn.of(
                tool_calls=[
                    ToolCall(TOOL_NAME, {"text": text}, id=call_id)
                    for call_id, text in zip(CALL_IDS, TOOL_TEXTS, strict=True)
                ]
            ),
            Turn.of(text="done"),
        )
        session = await probe.session(model=MODEL, tools=[f"steer-probe.{TOOL_NAME}"])
        media = (await session.driver.http.upload_media(session.session_id, PNG))[
            "media"
        ]
        second_media = (
            await session.driver.http.upload_media(session.session_id, SECOND_PNG)
        )["media"]
        async with _subscribe(probe, session) as observer:
            sync = await observer.watch.expect("sync_session")
            assert sync.data["pending_user_messages"] == []

            start_id = await session.send("begin")
            await host.wait_for_call(TOOL_NAME)
            start = await observer.watch.expect("user_message_committed")
            started = await observer.watch.expect("turn_started")
            assert start.data["origin_request_ids"] == [start_id]
            assert start.index < started.index

            text_id = await session.send("same")
            await observer.watch.expect(
                "user_message_queued", where={"origin_request_id": text_id}
            )
            http_id = "http-image"
            result = await session.driver.http.request(
                "POST",
                "/api/session/send",
                body={
                    "session_id": session.session_id,
                    "request_id": http_id,
                    "content": "",
                    "media": [media],
                },
            )
            assert result == {"ok": True, "request_id": http_id}
            await observer.watch.expect(
                "user_message_queued", where={"origin_request_id": http_id}
            )
            mixed_id = await session.send("same", media=[second_media])
            await observer.watch.expect(
                "user_message_queued", where={"origin_request_id": mixed_id}
            )
            await session.watch.expect(
                "user_message_queued", where={"origin_request_id": mixed_id}
            )
            pending = [
                {"origin_request_id": text_id, "content": "same", "media": []},
                {"origin_request_id": http_id, "content": "", "media": [media]},
                {
                    "origin_request_id": mixed_id,
                    "content": "same",
                    "media": [second_media],
                },
            ]
            assert len(probe.requests_for(MODEL)) == 1
            assert not observer.watch.events("tool_call_result")
            for subscriber in (session, observer):
                observed = [
                    {
                        key: event.data[key]
                        for key in ("origin_request_id", "content", "media")
                    }
                    for event in subscriber.watch.events("user_message_queued")
                ]
                assert observed == [
                    {"origin_request_id": start_id, "content": "begin", "media": []},
                    *pending,
                ]

            release.set()
            await _wait_for_file(probe.env.root / "hook-entered")
            with pytest.raises(DriverHttpError) as rejected:
                await session.rewind(start.data["message_uuid"])
            assert rejected.value.call.status == 409
            assert (await session.rewind("current"))["ok"]
            current = await observer.watch.expect("sync_session")
            assert current.data["pending_user_messages"] == pending
            async with _subscribe(probe, session) as joined:
                sync = await joined.watch.expect("sync_session")
                assert sync.data["pending_user_messages"] == pending
                assert [msg["role"] for msg in sync.data["messages"]] == [
                    "user",
                    "assistant",
                    "tool",
                    "tool",
                ]
                assert "uncommitted" not in sync.data
                (probe.env.root / "hook-release").touch()
                joined_commit = await joined.watch.expect("user_message_committed")
                await joined.watch.expect("done")

            commit = await observer.watch.expect("user_message_committed")
            assert commit.data == joined_commit.data
            assert commit.data["origin_request_ids"] == [text_id, http_id, mixed_id]
            assert commit.data["content"] == "hooked: same\nsame"
            assert commit.data["media"] == [media, second_media]
            await observer.watch.expect("done")
            await session.watch.expect("done")
            for subscriber in (session, observer):
                results = subscriber.watch.events("tool_call_result")
                commits = subscriber.watch.events("user_message_committed")
                assert len(commits) == 2 and commits[-1].data == commit.data
                assert len(results) == 2 and all(
                    event.index < commits[-1].index for event in results
                )
                assert {
                    event.data["tool_call_id"]: event.data["tool_result"]
                    for event in results
                } == dict(zip(CALL_IDS, TOOL_TEXTS, strict=True))
                assert commits[-1].index < subscriber.watch.events("text")[0].index
                assert len(subscriber.watch.events("turn_started")) == 1
                assert [
                    e.data["origin_request_id"]
                    for e in subscriber.watch.events("user_message_accepted")
                ] == [start_id, text_id, http_id, mixed_id]
                subscriber.watch.assert_never(["error", "user_messages_dropped"])

        # A new connection rebuilds the committed item from history using the same UUID.
        async with _subscribe(probe, session) as reconnected:
            sync = await reconnected.watch.expect("sync_session")
            assert sync.data["pending_user_messages"] == []
            assert sync.data["messages"] == (await session.get())["messages"]
            assert [m["role"] for m in sync.data["messages"]] == [
                "user",
                "assistant",
                "tool",
                "tool",
                "user",
                "assistant",
            ]
            matches = [
                m
                for m in sync.data["messages"]
                if m["uuid"] == commit.data["message_uuid"]
            ]
            assert matches == [
                {
                    "uuid": commit.data["message_uuid"],
                    "role": "user",
                    "content": commit.data["content"],
                    "media": commit.data["media"],
                }
            ]

        messages = probe.request(MODEL, 1).body["messages"]
        if protocol == "anthropic":
            assert probe.request(MODEL, 1).path == "/v1/messages"
            assert [m["role"] for m in messages] == ["user", "assistant", "user"]
            assert [block["id"] for block in messages[-2]["content"]] == CALL_IDS
            blocks = messages[-1]["content"]
            assert [b["type"] for b in blocks] == [
                "tool_result",
                "tool_result",
                "text",
                "image",
                "image",
            ]
            assert [b["tool_use_id"] for b in blocks[:2]] == CALL_IDS
            assert [b["content"] for b in blocks[:2]] == TOOL_TEXTS
            data = [b["source"]["data"] for b in blocks[3:]]
        else:
            context = probe.context(MODEL, 1)
            context.assert_tool_pairing()
            assert context.role_sequence() == [
                "user",
                "assistant",
                "tool",
                "tool",
                "user",
            ]
            assert [m.tool_call_id for m in context.messages_of("tool")] == CALL_IDS
            assert context.texts("tool") == TOOL_TEXTS
            blocks = messages[-1]["content"]
            assert [b["type"] for b in blocks] == ["text", "image_url", "image_url"]
            data = [b["image_url"]["url"].split(",", 1)[1] for b in blocks[1:]]
        assert [b["text"] for b in blocks if b["type"] == "text"] == [
            commit.data["content"]
        ]
        assert [base64.b64decode(image) for image in data] == [PNG, SECOND_PNG]
        view = probe.history(session)
        disk_messages = [
            m for m in view.records if m.get("uuid") == commit.data["message_uuid"]
        ]
        assert len(disk_messages) == 1
        disk_message = disk_messages[0]
        assert disk_message["role"] == "user"
        assert disk_message["content"] == commit.data["content"]
        assert disk_message["media"] == [
            {k: value for k, value in ref.items() if value is not None}
            for ref in commit.data["media"]
        ]
        history = view.messages()
        assert [
            (m["tool_call_id"], m["content"]) for m in history if m["role"] == "tool"
        ] == list(zip(CALL_IDS, TOOL_TEXTS, strict=True))
    finally:
        await host.close()


@pytest.mark.probe_env(models=[MODEL])
async def test_tool_results_without_pending_input(probe: Probe) -> None:
    probe.register(
        MODEL,
        Turn.of(tool_calls=[ToolCall("Write", {"path": "plain.txt", "content": "ok"})]),
        Turn.of(text="done"),
    )
    session = await probe.session(model=MODEL)
    assert (await session.chat("write the file")).data["subtype"] == "success"
    probe.files.assert_content("plain.txt", equals="ok")
    (result,) = session.watch.events("tool_call_result")
    (tool,) = probe.context(MODEL, 1).messages_of("tool")
    assert tool.content == result.data["tool_result"]
    (accepted,) = session.watch.events("user_message_accepted")
    assert accepted.data["content"] == "write the file"
    assert accepted.index < result.index
    assert len(session.watch.events("user_message_committed")) == 1


def _ask_call() -> ToolCall:
    return ToolCall(
        "AskUserQuestion",
        {
            "questions": [
                {
                    "id": "choice",
                    "header": "Choice",
                    "question": "Which option?",
                    "options": [{"label": "one"}, {"label": "two"}],
                }
            ]
        },
        id="call_ask",
    )


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.parametrize("boundary", ["disabled", "max_turns", "interrupt"])
async def test_steer_respects_turn_boundaries(probe: Probe, boundary: str) -> None:
    if boundary == "disabled":
        config = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
        config["steer"] = False
        probe.env.config_path.write_text(yaml.safe_dump(config), encoding="utf-8")
        result = await probe.driver_required.http.request("POST", "/api/system/reload")
        assert result["ok"], result

    turns = [Turn.of(tool_calls=[_ask_call()])]
    if boundary == "disabled":
        turns.append(Turn.of(text="tool follow-up"))
    turns.append(Turn.of(text="next turn"))
    probe.register(MODEL, *turns)
    session = await probe.session(
        model=MODEL, max_turns=1 if boundary == "max_turns" else None
    )
    send = await session.driver.http.request(
        "POST",
        "/api/session/send",
        body={"session_id": session.session_id, "content": "begin"},
    )
    assert send["ok"] and send["request_id"]
    await session.watch.expect(
        "user_message_queued", where={"origin_request_id": send["request_id"]}
    )
    ask = await session.watch.expect("ask")
    queued_id = await session.send("later")
    await session.watch.expect(
        "user_message_queued", where={"origin_request_id": queued_id}
    )

    if boundary == "interrupt":
        await session.interrupt()
        interrupted = await session.watch.expect("interrupted")
        assert interrupted.data["dropped_request_ids"] == [queued_id]
        session.watch.assert_never("user_messages_dropped")
        assert len(probe.requests_for(MODEL)) == 1
        next_id = await session.send("after interrupt")
    else:
        answer_id = await session.answer(
            ask, "" if boundary == "disabled" else "Choice: one"
        )
        finished = await session.watch.expect("turn_result")
        assert finished.data["subtype"] == (
            "error_max_turns" if boundary == "max_turns" else "success"
        )
        session.watch.assert_never(
            "user_message_queued", where={"origin_request_id": answer_id}
        )
        session.watch.assert_never(
            "user_message_accepted", where={"origin_request_id": answer_id}
        )
        next_id = queued_id

    await session.watch.expect(
        "user_message_committed", where={"origin_request_ids": [next_id]}
    )
    await session.watch.expect("turn_result", where={"result": "next turn"})
    requests = probe.requests_for(MODEL)
    assert len(requests) == (3 if boundary == "disabled" else 2)
    follow_up = requests[-1].context()
    follow_up.assert_tool_pairing()
    (tool,) = follow_up.messages_of("tool")
    assert tool.tool_call_id == "call_ask"
    assert follow_up.messages[-1].content == (
        "after interrupt" if boundary == "interrupt" else "later"
    )
    if boundary == "disabled":
        assert requests[1].context().role_sequence() == ["user", "assistant", "tool"]
    if boundary == "interrupt":
        assert tool.content == "Tool call interrupted by user."
        assert "later" not in follow_up.texts("user")


@pytest.mark.probe_env(models=[MODEL], hooks=["hooks/*.py"])
@pytest.mark.parametrize("replace_agent", [False, True])
async def test_cancel_confirms_preparing_and_queued_input(
    probe: Probe, replace_agent: bool
) -> None:
    await _install_hook(probe)
    probe.register(MODEL, Turn.of(text="recovered"))
    session = await probe.session(model=MODEL)
    preparing_content = "hold swap" if replace_agent else "hold"
    preparing_id = await session.send(preparing_content)
    await _wait_for_file(probe.env.root / "hook-entered")
    queued_id = await session.send("later")
    await session.watch.expect(
        "user_message_queued", where={"origin_request_id": queued_id}
    )

    async with _subscribe(probe, session) as observer:
        sync = await observer.watch.expect("sync_session")
        assert sync.data["messages"] == []
        assert sync.data["pending_user_messages"] == [
            {
                "origin_request_id": preparing_id,
                "content": preparing_content,
                "media": [],
            },
            {"origin_request_id": queued_id, "content": "later", "media": []},
        ]
        if replace_agent:
            http = session.driver.http
            replacement = asyncio.create_task(
                http.request(
                    "POST",
                    "/api/session/update",
                    body={
                        "session_id": session.session_id,
                        "agent": "default",
                        "model_id": MODEL,
                    },
                )
            )
            try:
                await _wait_for_file(probe.env.root / "hook-cancelled")
                with pytest.raises(DriverHttpError) as failure:
                    await http.request(
                        "POST",
                        "/api/session/send",
                        body={
                            "session_id": session.session_id,
                            "request_id": "during-swap",
                            "content": "late",
                        },
                    )
                assert failure.value.call.status == 409
            finally:
                (probe.env.root / "hook-release").touch()
                assert (await replacement)["ok"]
            observer.watch.assert_never(
                "user_message_queued", where={"origin_request_id": "during-swap"}
            )
        else:
            await session.interrupt()
        dropped = await observer.watch.expect("user_messages_dropped")
        assert dropped.data["origin_request_ids"] == [preparing_id]
        interrupted = await observer.watch.expect("interrupted")
        assert interrupted.data["dropped_request_ids"] == [preparing_id, queued_id]
        observer.watch.assert_never(["user_message_committed", "turn_started"])
        assert not probe.requests_for(MODEL)

    async with _subscribe(probe, session) as observer:
        sync = await observer.watch.expect("sync_session")
        assert sync.data["messages"] == sync.data["pending_user_messages"] == []
    assert (await session.chat("try again")).data["result"] == "recovered"


@pytest.mark.probe_env(models=[MODEL])
async def test_empty_ordinary_input_is_rejected_on_both_transports(
    probe: Probe,
) -> None:
    probe.register(MODEL, Turn.of(text="recovered"))
    session = await probe.session(model=MODEL)
    with pytest.raises(DriverHttpError) as failure:
        await session.driver.http.request(
            "POST",
            "/api/session/send",
            body={
                "session_id": session.session_id,
                "content": "",
                "request_id": "http-empty",
            },
        )
    assert failure.value.call.status == 400
    empty_id = await session.send("")
    event = await session.watch.expect("error", where={"request_id": empty_id})
    assert "content or media" in event.data["message"]
    assert (await session.get())["messages"] == []
    session.watch.assert_never(["user_message_queued", "user_message_committed"])
    assert (await session.chat("valid")).data["result"] == "recovered"


@pytest.mark.probe_env(models=[VISION_MODEL])
async def test_rewind_to_steer_preserves_tool_media(probe: Probe) -> None:
    (probe.workspace / "tool.png").write_bytes(PNG)
    probe.register(
        MODEL,
        Turn.of(
            tool_calls=[
                _ask_call(),
                ToolCall("ReadImage", {"path": "tool.png"}, id="call_image"),
            ]
        ),
        Turn.of(text="steered"),
        Turn.of(text="after rewind"),
    )
    session = await probe.session(model=MODEL, tools=["AskUserQuestion", "ReadImage"])
    await session.send("begin")
    ask = await session.watch.expect("ask")
    steer_id = await session.send("follow up")
    await session.watch.expect(
        "user_message_queued", where={"origin_request_id": steer_id}
    )
    await session.answer(ask, "Choice: one")
    committed = await session.watch.expect(
        "user_message_committed", where={"origin_request_ids": [steer_id]}
    )
    await session.watch.expect("done")

    target = committed.data["message_uuid"]
    before = probe.history(session)
    image_tool = before.messages()[-3]
    assert image_tool["tool_call_id"] == "call_image"
    assert image_tool["media"]
    assert (await session.rewind(target))["draft"] == "follow up"
    after = probe.history(session)
    material = assert_rewind_transition(before, after, target)
    assert material["source_uuid"] == image_tool["uuid"]
    tool = after.messages()[-1]
    assert [tool[key] for key in ("content", "tool_call_id", "media")] == [
        image_tool[key] for key in ("content", "tool_call_id", "media")
    ]
    async with _subscribe(probe, session) as reader:
        sync = await reader.watch.expect("sync_session")
        assert sync.data["messages"][-1]["media"][0]["id"] == tool["media"][0]["id"]
    assert (await session.chat("continue")).data["result"] == "after rewind"
    context = probe.context(MODEL, 2)
    context.assert_tool_pairing()
    tool_results = context.messages_of("tool")
    assert [(m.tool_call_id, m.content) for m in tool_results][-1] == (
        "call_image",
        image_tool["content"],
    )
    images = [
        part["image_url"]["url"]
        for message in context.body["messages"]
        if isinstance(message["content"], list)
        for part in message["content"]
        if part.get("type") == "image_url"
    ]
    assert [base64.b64decode(url.split(",", 1)[1]) for url in images] == [PNG]


@pytest.mark.probe_env(models=[MODEL], hooks=["hooks/*.py"])
async def test_request_ids_are_bounded_and_unique_per_loaded_session(
    probe: Probe,
) -> None:
    await _install_hook(probe)
    probe.register(
        MODEL,
        Turn.of(text="first"),
        Turn.of(text="second"),
        Turn.of(text="other session"),
    )
    session = await probe.session(model=MODEL)
    sid = session.session_id
    http = session.driver.http

    async def send_http(request_id: str, content: str) -> dict:
        return await http.request(
            "POST",
            "/api/session/send",
            body={
                "session_id": sid,
                "request_id": request_id,
                "content": content,
            },
        )

    async with _subscribe(probe, session) as sender:
        await sender.watch.expect("sync_session")
        for bad_id in ("", "non/ascii", "带空格", "with space", "too_long_" * 8):
            with pytest.raises(DriverHttpError) as failure:
                await send_http(bad_id, "rejected")
            assert failure.value.call.status == 400
            await sender.driver.websocket.send_request(
                sid, "rejected", request_id=bad_id
            )
            error = await sender.driver.timeline.wait_for(
                lambda event: event.type == "error", within=5
            )
            assert error is not None
            assert "request_id must be" in error.data["message"]
            assert error.data["request_id"] != bad_id
            sender.driver.timeline.set_cursor(error.index + 1)

        async def reject_duplicate(request_id: str) -> None:
            with pytest.raises(DriverHttpError) as failure:
                await send_http(request_id, "duplicate HTTP input")
            assert failure.value.call.status == 400
            await sender.driver.websocket.send_request(
                sid, "duplicate WS input", request_id=request_id
            )
            event = await sender.watch.expect("error", where={"request_id": request_id})
            assert event.data["message"] == "request_id already used in this session"

        preparing_id = "prepare_" + "A" * 56
        assert (await send_http(preparing_id, "hold"))["request_id"] == preparing_id
        await _wait_for_file(probe.env.root / "hook-entered")
        await reject_duplicate(preparing_id)
        queued_id = await session.driver.websocket.send_request(
            sid, "next", request_id="queued-01"
        )
        await session.watch.expect(
            "user_message_queued", where={"origin_request_id": queued_id}
        )
        await reject_duplicate(queued_id)
        (probe.env.root / "hook-release").touch()
        await session.watch.expect("turn_result", where={"result": "second"})
        # Recent IDs survive a replacement of the session's running agent.
        await http.request(
            "POST",
            "/api/session/update",
            body={
                "session_id": sid,
                "agent": "default",
                "model_id": MODEL,
            },
        )
        await reject_duplicate(preparing_id)
        await reject_duplicate(queued_id)
        session.watch.assert_never("error")
        assert [e.data["request_id"] for e in session.watch.events("delivered")] == [
            preparing_id,
            queued_id,
        ]
        assert [
            e.data["origin_request_id"]
            for e in session.watch.events("user_message_queued")
        ] == [preparing_id, queued_id]
        assert probe.context(MODEL, 1).texts("user") == ["hooked: hold", "next"]

    other = await probe.session(model=MODEL)
    await other.driver.websocket.send_request(
        other.session_id, "different session", request_id=preparing_id
    )
    assert (await other.watch.expect("turn_result")).data["result"] == "other session"
