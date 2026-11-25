"""Correlate live message facts with the active history over public HTTP and WS."""

from __future__ import annotations

import asyncio
import json
from collections.abc import Sequence
from datetime import UTC, datetime, timedelta
from typing import Any

import pytest
from aiohttp import web
from _session_helpers import evict_and_wait_inactive

from wing_probe import Driver, Probe, Session, ToolCall, Turn, assert_fork_of
from wing_probe.provider.sse import SSEFrame

FIRST = {"id": "history-a", "name": "probe/history-a", "display_name": "History A"}
SECOND = {"id": "history-b", "name": "probe/history-b", "display_name": "History B"}
MODEL_FIELDS = ("model", "model_id", "provider_name", "model_display_name")
MESSAGE_FIELDS = ("message_id", "message_at")
ASK_ARGS = {
    "questions": [
        {
            "id": "choice",
            "header": "Choice",
            "question": "Continue?",
            "options": [{"label": "continue"}, {"label": "stop"}],
        }
    ]
}

pytestmark = [pytest.mark.asyncio, pytest.mark.probe_env(models=[FIRST, SECOND])]


async def _history(session: Session) -> dict:
    return await session.driver.http.request(
        "GET", "/api/session/get", params={"session_id": session.session_id}
    )


def _assert_fields(actual: dict, expected: dict, fields: tuple[str, ...]) -> None:
    assert {field: actual[field] for field in fields} == {
        field: expected[field] for field in fields
    }


def _assert_model(message: dict, spec: dict) -> None:
    assert tuple(message[field] for field in MODEL_FIELDS) == (
        spec["name"],
        spec["id"],
        "probe",
        spec["display_name"],
    )


def _utc(value: str) -> datetime:
    parsed = datetime.fromisoformat(value)
    assert parsed.utcoffset() == timedelta(0), value
    return parsed


async def test_live_waiting_and_restored_history_agree(probe: Probe) -> None:
    bad_args = {"path": "missing.txt"}
    (probe.workspace / "present.txt").write_text("readable\n", encoding="utf-8")
    probe.register(
        FIRST["name"],
        Turn.of(
            thinking="checking files",
            text="I will check.",
            chunk=3,
            tool_calls=[
                ToolCall("Read", bad_args, id="call_bad", cut=(3, 9)),
                ToolCall("Read", {"path": "present.txt"}, id="call_ok"),
                ToolCall("AskUserQuestion", ASK_ARGS, id="call_ask"),
            ],
        ),
        Turn.of(text="first answer"),
    )
    probe.register(SECOND["name"], Turn.of(text="second model answer"))
    session = await probe.session(model=FIRST["id"], tools=["Read", "AskUserQuestion"])
    await session.watch.expect("sync_session")

    start_id = await session.send("inspect both files")
    user = (await session.watch.expect("user_message_committed")).data
    started = (await session.watch.expect("turn_started")).data
    assistant = (await session.watch.expect("assistant_turn")).data
    ask = await session.watch.expect("ask")
    queued_id = await session.send("also consider this")
    queued = await session.watch.expect(
        "user_message_queued", where={"origin_request_id": queued_id}
    )
    assert "message_at" not in queued.data

    for call_id in ("call_bad", "call_ok"):
        result = await session.timeline.wait_for(
            lambda event: event.matches(
                ("tool_call_result",), {"tool_call_id": call_id}
            ),
            within=5,
            from_cursor=False,
        )
        assert result is not None, f"Missing tool result for {call_id}"

    second = await Driver.connect(probe.env)
    try:
        joined = await second.attach(session.session_id)
        await joined.watch.expect("sync_session")
    finally:
        await second.close()
    reconnected = await Driver.connect(probe.env)
    try:
        joined = await reconnected.attach(session.session_id)
        waiting = (await joined.watch.expect("sync_session")).data
        http_waiting = await _history(session)
        for field in (
            "messages",
            "uncommitted",
            "uncommitted_tools",
            "events",
            "pending_user_messages",
            "turn_started_at",
            "status",
        ):
            assert waiting[field] == http_waiting[field]
        assert waiting["status"] == "waiting"
        assert waiting["turn_started_at"] == started["turn_started_at"]
        assert [
            item["origin_request_id"] for item in waiting["pending_user_messages"]
        ] == [queued_id]
        assert user["origin_request_ids"] == [start_id]
        (committed_user,) = waiting["messages"]
        _assert_fields(committed_user, user, (*MESSAGE_FIELDS, "content"))
        assert committed_user["uuid"] == user["message_uuid"]
        assert _utc(started["turn_started_at"]) <= _utc(user["message_at"])
        assert _utc(user["message_at"]) <= _utc(assistant["message_at"])

        uncommitted = waiting["uncommitted"]
        _assert_model(uncommitted, FIRST)
        _assert_fields(uncommitted, assistant, (*MESSAGE_FIELDS, *MODEL_FIELDS))
        assert assistant["uuid"] == assistant["message_id"]
        assert uncommitted["content"] == "I will check."
        assert uncommitted["reasoning_content"] == "checking files"
        assert uncommitted["tool_calls"][0] == {
            "id": "call_bad",
            "name": "Read",
            "arguments": bad_args,
        }
        fragments = [
            event.data["args_fragment"]
            for event in session.watch.events("tool_call_stream")
            if event.data["tool_call_id"] == "call_bad" and not event.data["is_final"]
        ]
        assert len(fragments) >= 3
        assert json.loads("".join(fragments)) == bad_args

        result_by_call = {
            item["tool_call_id"]: item for item in waiting["uncommitted_tool_results"]
        }
        assert result_by_call == {
            item["tool_call_id"]: item
            for item in http_waiting["uncommitted_tool_results"]
        }
        assert {
            call_id: item["tool_status"] for call_id, item in result_by_call.items()
        } == {
            "call_bad": "error",
            "call_ok": "success",
        }
        assert [
            event["tool_call_id"]
            for event in waiting["events"]
            if event["type"] == "ask"
        ] == [ask.data["tool_call_id"]]
        await joined.answer(ask, "choice: continue")
        assert (await joined.watch.expect("turn_result")).data["subtype"] == "success"
        await joined.watch.expect("done")
    finally:
        await reconnected.close()

    assert (await session.watch.expect("done")).data["session_id"] == session.session_id
    queued_commit = [
        event.data
        for event in session.watch.events("user_message_committed")
        if event.data["origin_request_ids"] == [queued_id]
    ]
    assert len(queued_commit) == 1
    assert _utc(queued_commit[0]["message_at"]) >= _utc(assistant["message_at"])

    await session.driver.http.request(
        "POST",
        "/api/session/update",
        body={"session_id": session.session_id, "model_id": SECOND["id"]},
    )
    assert (await session.chat("switch check")).data["subtype"] == "success"
    await session.watch.expect("done")
    saved = await _history(session)
    rows = saved["messages"]
    assert [row["role"] for row in rows] == [
        "user",
        "assistant",
        "tool",
        "tool",
        "tool",
        "user",
        "assistant",
        "user",
        "assistant",
    ]
    assert len({row["message_id"] for row in rows}) == len(rows)
    live_by_id = {
        event.data["message_id"]: event.data
        for kind in ("user_message_committed", "assistant_turn", "tool_call_result")
        for event in session.watch.events(kind)
    }
    assert set(live_by_id) == {row["message_id"] for row in rows}
    assert saved["uncommitted_tool_results"] == []
    assert saved["pending_user_messages"] == []
    assert all(event["type"] != "ask" for event in saved["events"])
    for row in rows:
        _assert_fields(row, live_by_id[row["message_id"]], MESSAGE_FIELDS)
        _utc(row["message_at"])
        if row["role"] == "assistant":
            _assert_fields(row, live_by_id[row["message_id"]], MODEL_FIELDS)
    _assert_model(rows[1], FIRST)
    _assert_model(rows[6], FIRST)
    _assert_model(rows[-1], SECOND)
    for row in rows[2:5]:
        live = live_by_id[row["message_id"]]
        assert row["content"] == live["tool_result"]
        _assert_fields(row, live, ("tool_call_id", "tool_status", "duration_ms"))
    _assert_fields(rows[2], result_by_call["call_bad"], MESSAGE_FIELDS)
    _assert_fields(rows[5], queued_commit[0], (*MESSAGE_FIELDS, "content"))
    assert (
        rows[1]["tool_calls"][0]["arguments"]
        == live_by_id[rows[2]["message_id"]]["tool_args"]
    )
    probe.context(FIRST["name"], index=1).assert_tool_pairing()

    log = session.session_dir / "history.jsonl"
    before = log.read_bytes()
    await evict_and_wait_inactive(probe, session.session_id)
    assert log.read_bytes() == before
    fresh = await Driver.connect(probe.env)
    try:
        restored = await fresh.resume(session.session_id)
        snapshot = (await restored.watch.expect("sync_session")).data
        assert snapshot["messages"] == (await _history(restored))["messages"] == rows
        assert snapshot["events"] == saved["events"]
        assert snapshot["status"] == "idle"
        assert "turn_started_at" not in snapshot
    finally:
        await fresh.close()
    assert log.read_bytes() == before


async def test_interrupted_stream_retains_identity_and_notification_position(
    probe: Probe,
) -> None:
    content = "streamed partial content " * 80
    probe.register(FIRST["name"], Turn.of(text=content, chunk=4, delay=0.05))
    session = await probe.session(model=FIRST["id"])
    await session.watch.expect("sync_session")
    await session.send("stream this")
    await session.watch.expect("text")

    midstream = (await _history(session))["uncommitted"]
    assert midstream["message_id"]
    assert "message_at" not in midstream
    _assert_model(midstream, FIRST)
    assert (await session.interrupt())["ok"]
    partial = (await session.watch.expect("assistant_message_committed")).data
    interrupted = (await session.watch.expect("interrupted")).data
    after = await _history(session)
    user, kept = after["messages"]
    _assert_fields(partial, kept, (*MESSAGE_FIELDS, *MODEL_FIELDS, "stop_reason"))
    assert kept["message_id"] == midstream["message_id"]
    assert kept["stop_reason"] == "interrupted"
    assert partial["content_blocks"] == [{"type": "text", "text": kept["content"]}]
    assert content.startswith(kept["content"]) and kept["content"] != content
    assert _utc(kept["message_at"]) >= _utc(user["message_at"])
    assert interrupted["after_message_id"] == kept["message_id"]
    assert interrupted == next(
        event for event in after["events"] if event["type"] == "interrupted"
    )

    child = await session.fork("current")
    assert_fork_of(session.history, child.history, "current")
    child_history = await _history(child)
    forked = child_history["messages"][-1]
    assert forked["uuid"] != kept["uuid"]
    _assert_fields(forked, kept, (*MESSAGE_FIELDS, *MODEL_FIELDS, "content"))
    assert child_history["events"][-1]["after_message_id"] == forked["message_id"]


async def test_legacy_records_keep_unknown_message_facts(probe: Probe) -> None:
    probe.register(
        FIRST["name"],
        Turn.of(tool_calls=[ToolCall("Read", {"path": "missing.txt"})]),
        Turn.of(text="finished"),
    )
    session = await probe.session(model=FIRST["id"], tools=["Read"])
    assert (await session.chat("old turn")).data["subtype"] == "success"
    await evict_and_wait_inactive(probe, session.session_id)

    path = session.session_dir / "history.jsonl"
    new_fields = {*MESSAGE_FIELDS, *MODEL_FIELDS, "tool_status", "duration_ms"}
    records = [
        json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()
    ]
    for record in records:
        if record["role"] != "event":
            for name in new_fields:
                record.pop(name, None)
    path.write_text(
        "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
    )
    before = path.read_bytes()
    fresh = await Driver.connect(probe.env)
    try:
        restored = await fresh.resume(session.session_id)
        snapshot = (await restored.watch.expect("sync_session")).data
        assert snapshot["messages"] == (await _history(restored))["messages"]
        assert [row["role"] for row in snapshot["messages"]] == [
            "user",
            "assistant",
            "tool",
            "assistant",
        ]
        for row in snapshot["messages"]:
            assert row["message_id"] == row["uuid"]
            assert not (new_fields - {"message_id"}) & row.keys()
    finally:
        await fresh.close()
    assert path.read_bytes() == before


@pytest.mark.probe_env(models=[FIRST, SECOND], provider_extra={"max_retries": 0})
async def test_terminal_generation_error_replays_at_the_failed_turn(
    probe: Probe,
) -> None:
    probe.register(
        FIRST["name"],
        Turn.of(thinking="no completed answer"),
        Turn.of(text="recovered"),
    )
    session = await probe.session(model=FIRST["id"])
    await session.watch.expect("sync_session")
    await session.send("failed turn")
    failed_user = (await session.watch.expect("user_message_committed")).data
    error = (await session.watch.expect("error")).data
    await session.watch.expect("done")
    assert error["after_message_id"] == failed_user["message_id"]

    assert (await session.chat("next turn")).data["subtype"] == "success"
    await session.watch.expect("done")
    snapshot = await _history(session)
    assert snapshot["events"] == [error]
    assert [row["content"] for row in snapshot["messages"]] == [
        "failed turn",
        "next turn",
        "recovered",
    ]


async def test_generation_timestamp_precedes_openai_transport_tail(
    probe: Probe, monkeypatch: pytest.MonkeyPatch
) -> None:
    reached_tail, release_tail = asyncio.Event(), asyncio.Event()
    write_frames = probe.provider._write_frames

    async def hold_done(
        response: web.StreamResponse, frames: Sequence[SSEFrame], **kwargs: Any
    ) -> None:
        await write_frames(response, frames[:-1], **kwargs)
        assert frames[-1].data is None
        reached_tail.set()
        await release_tail.wait()
        await response.write(frames[-1].encode())

    monkeypatch.setattr(probe.provider, "_write_frames", hold_done)
    probe.register(FIRST["name"], Turn.of(text="complete before transport tail"))
    session = await probe.session(model=FIRST["id"])
    await session.watch.expect("sync_session")
    await session.send("produce an answer")
    try:
        async with asyncio.timeout(8):
            await reached_tail.wait()
            while True:
                mid = (await _history(session))["uncommitted"]
                if mid and "message_at" in mid:
                    break
                await asyncio.sleep(0.02)
        assert mid["content"] == "complete before transport tail"
        session.watch.assert_never("assistant_turn")
        assert _utc(mid["message_at"]) <= datetime.now(UTC)
    finally:
        release_tail.set()
    final = (await session.watch.expect("assistant_turn")).data
    await session.watch.expect("done")
    committed = (await _history(session))["messages"][-1]
    _assert_fields(final, mid, MESSAGE_FIELDS)
    _assert_fields(committed, mid, MESSAGE_FIELDS)


@pytest.mark.probe_env(
    models=[FIRST, SECOND],
    provider_extra={"protocol": "anthropic", "max_retries": 1, "max_retry_delay": 0.05},
)
async def test_interrupted_transport_retry_uses_the_current_attempt_time(
    probe: Probe, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = asyncio.Event()
    write_frames = probe.provider._write_frames

    async def retry_after_stop(
        response: web.StreamResponse,
        frames: Sequence[SSEFrame],
        *,
        request_index: int,
        **kwargs: Any,
    ) -> None:
        if request_index == 0:
            assert frames[-1].event == "message_stop"
            await write_frames(response, frames, request_index=request_index, **kwargs)
            await response.write(
                SSEFrame(
                    {
                        "type": "error",
                        "error": {
                            "type": "overloaded_error",
                            "message": "retry fixture",
                        },
                    },
                    event="error",
                ).encode()
            )
            return
        assert request_index == 1
        first_text = next(
            i for i, frame in enumerate(frames) if frame.kind == "content"
        )
        await write_frames(
            response, frames[: first_text + 1], request_index=request_index, **kwargs
        )
        await release.wait()

    monkeypatch.setattr(probe.provider, "_write_frames", retry_after_stop)
    probe.register(
        FIRST["name"], Turn.of(text="first attempt"), Turn.of(text="second attempt")
    )
    session = await probe.session(model=FIRST["id"])
    await session.watch.expect("sync_session")
    await session.send("retry test")
    try:
        await session.watch.expect(
            "text", where={"content": "second attempt"}, within=10
        )
        retry_started = datetime.now(UTC)
        mid = (await _history(session))["uncommitted"]
        assert mid["content"] == "second attempt"
        assert "message_at" not in mid
        assert (await session.interrupt())["ok"]
        partial = (await session.watch.expect("assistant_message_committed")).data
        assert _utc(partial["message_at"]) >= retry_started
        assert partial["content_blocks"] == [{"type": "text", "text": "second attempt"}]
        saved = (await _history(session))["messages"][-1]
        _assert_fields(partial, mid, ("message_id",))
        _assert_fields(saved, partial, MESSAGE_FIELDS)
    finally:
        release.set()


@pytest.mark.probe_env(
    models=[FIRST, SECOND],
    provider_extra={"protocol": "anthropic"},
    hooks=["hooks/*.py"],
)
async def test_sealed_stream_and_interrupted_tool_replay_the_executed_target(
    probe: Probe, monkeypatch: pytest.MonkeyPatch
) -> None:
    hooks = probe.env.root / "hooks"
    hooks.mkdir()
    (hooks / "rewrite.py").write_text(
        """
from wing.hooks import hooks

@hooks.on("before_tool_call")
def rewrite(call):
    if call.id == "call_wait":
        call.arguments["questions"][0]["question"] = "Rewritten question?"
""",
        encoding="utf-8",
    )
    assert (await probe.driver_required.http.reload())["ok"]
    write_frames = probe.provider._write_frames

    async def leave_tool_unfinished(
        response: web.StreamResponse, frames: Sequence[SSEFrame], **kwargs: Any
    ) -> None:
        await write_frames(
            response,
            [
                frame
                for frame in frames
                if not (
                    frame.event == "content_block_stop"
                    and (frame.data or {}).get("index") == 1
                )
            ],
            **kwargs,
        )

    monkeypatch.setattr(probe.provider, "_write_frames", leave_tool_unfinished)
    probe.register(
        FIRST["name"],
        Turn.of(
            tool_calls=[
                ToolCall("AskUserQuestion", ASK_ARGS, id="call_wait"),
                ToolCall(
                    "Read", {"path": "unfinished.txt"}, id="call_unfinished", cut=3
                ),
            ],
            finish="max_tokens",
        ),
        Turn.of(text="resumed"),
    )
    session = await probe.session(model=FIRST["id"], tools=["AskUserQuestion", "Read"])
    await session.watch.expect("sync_session")
    await session.send("start")
    live = (await session.watch.expect("assistant_turn")).data
    execution = (await session.watch.expect("tool_execution_started")).data
    ask = await session.watch.expect("ask")
    assert ask.data["questions"][0]["question"] == "Rewritten question?"
    assert [block["id"] for block in live["content_blocks"]] == ["call_wait"]
    observer = await Driver.connect(probe.env)
    try:
        joined = await observer.attach(session.session_id)
        snapshot = (await joined.watch.expect("sync_session")).data
        assert snapshot["uncommitted_tools"] == []
        assert (await _history(session))["uncommitted_tools"] == []
        assert execution["tool_call_id"] == "call_wait"
        assert (
            snapshot["tool_executions"]
            == (await _history(session))["tool_executions"]
            == {
                "call_wait": {
                    "tool_name": "AskUserQuestion",
                    "tool_args": execution["tool_args"],
                }
            }
        )
        pending = snapshot["uncommitted"]
        assert [call["id"] for call in pending["tool_calls"]] == ["call_wait"]
        _assert_fields(pending, live, MESSAGE_FIELDS)
        assert (await session.interrupt())["ok"]
        tool = (await joined.watch.expect("tool_call_result")).data
        assert tool["tool_status"] == "interrupted"
        assert tool["tool_args"] == execution["tool_args"]
        assert tool["tool_args"]["questions"][0]["question"] == "Rewritten question?"
        saved_assistant, saved_tool = (await _history(session))["messages"][1:]
        assert saved_assistant["tool_calls"][0]["arguments"] == ASK_ARGS
        assert saved_tool["tool_args"] == tool["tool_args"]
        _assert_fields(
            saved_tool, tool, (*MESSAGE_FIELDS, "tool_call_id", "tool_status")
        )
        assert (await session.chat("continue")).data["result"] == "resumed"
        probe.context(FIRST["name"], index=1).assert_tool_pairing()
    finally:
        await observer.close()


@pytest.mark.probe_env(
    models=[FIRST, SECOND],
    provider_extra={"protocol": "anthropic", "max_retries": 1, "max_retry_delay": 3},
)
@pytest.mark.parametrize("interrupt_backoff", [False, True])
async def test_retry_reset_reconciles_live_and_joined_subscribers(
    probe: Probe, monkeypatch: pytest.MonkeyPatch, interrupt_backoff: bool
) -> None:
    first_tail, first_failure, finish_second = (asyncio.Event() for _ in range(3))
    write_frames = probe.provider._write_frames

    async def control_retry(
        response: web.StreamResponse,
        frames: Sequence[SSEFrame],
        *,
        request_index: int,
        **kwargs: Any,
    ) -> None:
        if request_index == 0:
            assert frames[-1].event == "message_stop"
            await write_frames(response, frames, request_index=request_index, **kwargs)
            first_tail.set()
            await first_failure.wait()
            error = {"type": "overloaded_error", "message": "retry after message_stop"}
            await response.write(
                SSEFrame({"type": "error", "error": error}, event="error").encode()
            )
        elif request_index == 1:
            stop = (
                next(i for i, frame in enumerate(frames) if frame.kind == "tool_call")
                + 1
            )
            await write_frames(
                response, frames[:stop], request_index=request_index, **kwargs
            )
            await finish_second.wait()
            await write_frames(response, frames[stop:], request_index=request_index)
        else:
            await write_frames(response, frames, request_index=request_index, **kwargs)

    monkeypatch.setattr(probe.provider, "_write_frames", control_retry)
    call_id = "call_reused"
    (probe.workspace / "retained.txt").write_text("retained data\n", encoding="utf-8")
    second_call = ToolCall("Read", {"path": "retained.txt"}, id=call_id, cut=9)
    probe.register(
        FIRST["name"],
        Turn.of(
            text="abandoned answer",
            thinking="abandoned thinking",
            tool_calls=[
                ToolCall("Read", {"path": "abandoned.txt"}, id=call_id, cut=9),
            ],
        ),
        Turn.of(
            text="retained answer",
            thinking="retained thinking",
            tool_calls=[second_call],
        ),
        Turn.of(text="final answer"),
    )
    session = await probe.session(model=FIRST["id"], tools=["Read"])
    await session.watch.expect("sync_session")
    await session.send("read a file")
    observer = await Driver.connect(probe.env)
    try:
        await asyncio.wait_for(first_tail.wait(), timeout=5)
        async with asyncio.timeout(5):
            while True:
                original = await session.get()
                if original["uncommitted"] and original["uncommitted"].get(
                    "message_at"
                ):
                    break
                await asyncio.sleep(0.02)
        late = await observer.attach(session.session_id)
        first = (await late.watch.expect("sync_session")).data
        old_preview = first["uncommitted"]
        generation = first["assistant_generation"]
        message_id = old_preview["message_id"]
        assert (
            generation
            == original["assistant_generation"]
            == {
                "message_id": message_id,
                "generation_attempt": 1,
                "phase": "provisional",
            }
        )
        assert old_preview == original["uncommitted"]
        assert old_preview["content"] == "abandoned answer"
        assert old_preview["reasoning_content"] == "abandoned thinking"
        old_time = _utc(old_preview["message_at"])
        session.watch.assert_never("assistant_turn")

        first_failure.set()
        for client in (session, late):
            reset = await client.watch.expect("assistant_generation_reset")
            notice = await client.watch.expect("notice")
            assert reset.index < notice.index
            assert reset.data["message_id"] == message_id
            assert reset.data["generation_attempt"] == 1
        during_backoff = await session.get()
        await observer.http.subscribe(session.session_id, observer.client_id)
        rejoined = (await late.watch.expect("sync_session")).data
        expected = {**generation, "phase": "discarded"}
        assert probe.requests.count(FIRST["name"]) == 1
        assert (
            during_backoff["assistant_generation"]
            == rejoined["assistant_generation"]
            == expected
        )
        assert during_backoff["uncommitted"] is None and "uncommitted" not in rejoined
        assert (
            during_backoff["uncommitted_tools"] == rejoined["uncommitted_tools"] == []
        )
        if interrupt_backoff:
            assert (await session.interrupt())["ok"]
            for client in (session, late):
                await client.watch.expect("interrupted")
                client.watch.assert_never("assistant_message_committed")
            history = await session.get()
            assert history["assistant_generation"] is None
            assert [row["role"] for row in history["messages"]] == ["user"]
            assert probe.requests.count(FIRST["name"]) == 1
            return

        for client in (session, late):
            thinking = await client.watch.expect(
                "reasoning", where={"content": "retained thinking"}
            )
            text = await client.watch.expect(
                "text", where={"content": "retained answer"}
            )
            delta = await client.watch.expect(
                "tool_call_stream", where={"tool_call_id": call_id}
            )
            for event in (thinking, text, delta):
                assert event.data["message_id"] == message_id
                assert event.data["generation_attempt"] == 2
            assert delta.data["args_fragment"] == second_call.argument_chunks()[0]
        await observer.http.subscribe(session.session_id, observer.client_id)
        mid = (await late.watch.expect("sync_session")).data
        http = await session.get()
        for key in ("assistant_generation", "uncommitted", "uncommitted_tools"):
            assert mid[key] == http[key]
        assert mid["assistant_generation"] == {**generation, "generation_attempt": 2}
        assert mid["uncommitted"]["content"] == "retained answer"
        assert mid["uncommitted"]["reasoning_content"] == "retained thinking"
        assert "message_at" not in mid["uncommitted"]
        assert mid["uncommitted_tools"] == [
            {
                "tool_call_id": call_id,
                "tool_name": "Read",
                "args_fragment": second_call.argument_chunks()[0],
            }
        ]
        finish_second.set()
        for client in (session, late):
            boundary = (
                await client.watch.expect(
                    "assistant_turn", where={"message_id": message_id}
                )
            ).data
            assert boundary["generation_attempt"] == 2
            assert _utc(boundary["message_at"]) > old_time
            result = (
                await client.watch.expect(
                    "tool_call_result", where={"tool_call_id": call_id}
                )
            ).data
            assert result["tool_status"] == "success"
            await client.watch.expect("done")
        await observer.http.subscribe(session.session_id, observer.client_id)
        replay = (await late.watch.expect("sync_session")).data
        assert replay["messages"] == (await session.get())["messages"]
        (saved,) = [
            row for row in replay["messages"] if row["message_id"] == message_id
        ]
        _assert_fields(saved, boundary, MESSAGE_FIELDS)
        assert saved["content"] == "retained answer"
        assert saved["tool_calls"][0]["arguments"] == {"path": "retained.txt"}
    finally:
        first_failure.set()
        finish_second.set()
        await observer.close()


@pytest.mark.probe_env(models=[FIRST, SECOND], provider_extra={"protocol": "anthropic"})
@pytest.mark.parametrize("explicit_stop_reason", [True, False])
async def test_anthropic_samples_stop_reason_before_message_stop(
    probe: Probe, monkeypatch: pytest.MonkeyPatch, explicit_stop_reason: bool
) -> None:
    reached, release = asyncio.Event(), asyncio.Event()
    write_frames = probe.provider._write_frames

    async def hold_message_stop(
        response: web.StreamResponse, frames: Sequence[SSEFrame], **kwargs: Any
    ) -> None:
        assert frames[-1].event == "message_stop"
        assert frames[-2].event == "message_delta"
        await write_frames(
            response, frames[: -1 if explicit_stop_reason else -2], **kwargs
        )
        reached.set()
        await release.wait()
        await response.write(frames[-1].encode())

    monkeypatch.setattr(probe.provider, "_write_frames", hold_message_stop)
    probe.register(FIRST["name"], Turn.of(text="complete text"))
    session = await probe.session(model=FIRST["id"])
    await session.watch.expect("sync_session")
    await session.send("complete this")
    try:
        async with asyncio.timeout(5):
            await reached.wait()
            await session.watch.expect("text")
            while True:
                snapshot = await session.get()
                if not explicit_stop_reason or "message_at" in snapshot["uncommitted"]:
                    break
                await asyncio.sleep(0.02)
        preview = snapshot["uncommitted"]
        assert snapshot["assistant_generation"]["phase"] == "provisional"
        assert ("message_at" in preview) is explicit_stop_reason
        session.watch.assert_never("assistant_turn")
    finally:
        released_at = datetime.now(UTC)
        release.set()
    boundary = (await session.watch.expect("assistant_turn")).data
    await session.watch.expect("done")
    assert boundary["message_id"] == preview["message_id"]
    if explicit_stop_reason:
        assert boundary["message_at"] == preview["message_at"]
        assert _utc(boundary["message_at"]) <= released_at
    else:
        assert _utc(boundary["message_at"]) >= released_at
    _assert_fields((await session.get())["messages"][-1], boundary, MESSAGE_FIELDS)


@pytest.mark.probe_env(models=[FIRST, SECOND], hooks=["hooks/*.py"])
async def test_executing_read_exposes_the_actual_target_before_settlement(
    probe: Probe,
) -> None:
    hooks = probe.env.root / "hooks"
    hooks.mkdir()
    (hooks / "read_target.py").write_text(
        """
import asyncio
from pathlib import Path
from wing.hooks import hooks

@hooks.on("before_tool_call")
async def rewrite(call):
    if call.name == "Read":
        Path("read-rewriting").touch()
        while not Path("read-enter").exists():
            await asyncio.sleep(0.02)
        call.arguments["path"] = "executed.txt"

@hooks.on("after_tool_call")
async def hold(result, **kwargs):
    if kwargs["tool_name"] == "Read":
        Path("read-executed").write_text(result, encoding="utf-8")
        while not Path("read-release").exists():
            await asyncio.sleep(0.02)
""",
        encoding="utf-8",
    )
    assert (await probe.driver_required.http.reload())["ok"]
    (probe.workspace / "original.txt").write_text("original data\n", encoding="utf-8")
    (probe.workspace / "executed.txt").write_text("executed data\n", encoding="utf-8")
    original_args = {"path": "original.txt", "ignored_by_read": "value"}
    call_id = "call_target"
    probe.register(
        FIRST["name"],
        Turn.of(tool_calls=[ToolCall("Read", original_args, id=call_id)]),
        Turn.of(text="read completed"),
    )
    session = await probe.session(model=FIRST["id"], tools=["Read"])
    await session.watch.expect("sync_session")
    await session.send("read sample")
    observer = await Driver.connect(probe.env)
    enter = probe.env.root / "read-enter"
    release = probe.env.root / "read-release"
    try:
        legacy_start = await session.watch.expect("tool_call")
        assert legacy_start.data["tool_args"] == original_args
        async with asyncio.timeout(5):
            while not (probe.env.root / "read-rewriting").exists():
                await asyncio.sleep(0.02)
        assert (await session.get())["tool_executions"] == {}
        session.watch.assert_never("tool_execution_started")

        enter.touch()
        execution = (await session.watch.expect("tool_execution_started")).data
        assert execution["tool_call_id"] == legacy_start.data["tool_call_id"] == call_id
        actual = {"tool_name": "Read", "tool_args": {"path": "executed.txt"}}
        _assert_fields(execution, actual, ("tool_name", "tool_args"))
        executed = probe.env.root / "read-executed"
        async with asyncio.timeout(5):
            while not executed.exists():
                await asyncio.sleep(0.02)
        assert "executed data" in executed.read_text(encoding="utf-8")
        other = await observer.attach(session.session_id)
        snapshot = (await other.watch.expect("sync_session")).data
        http = await session.get()
        assert (
            snapshot["tool_executions"] == http["tool_executions"] == {call_id: actual}
        )
        assert snapshot["assistant_generation"] == http["assistant_generation"]
        assert snapshot["assistant_generation"]["phase"] == "finalized"
        assert (
            snapshot["uncommitted_tool_results"]
            == http["uncommitted_tool_results"]
            == []
        )
        assert snapshot["uncommitted"] == http["uncommitted"]
        assert snapshot["uncommitted"]["tool_calls"][0]["arguments"] == original_args

        release.touch()
        live_result = (await session.watch.expect("tool_call_result")).data
        joined_result = (await other.watch.expect("tool_call_result")).data
        assert live_result == joined_result
        assert live_result["tool_args"] == actual["tool_args"]
        assert live_result["tool_success"] is True
        await session.watch.expect("done")
        history = await session.get()
        assert history["tool_executions"] == {}
        assistant, tool = history["messages"][1:3]
        assert assistant["tool_calls"][0]["arguments"] == original_args
        assert tool["tool_status"] == "success"
        assert tool["tool_call_id"] == call_id
        _assert_fields(tool, live_result, (*MESSAGE_FIELDS, "tool_args"))
        assert "executed data" in tool["content"]
    finally:
        enter.touch()
        release.touch()
        await observer.close()
