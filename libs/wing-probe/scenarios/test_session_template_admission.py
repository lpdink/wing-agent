"""Opt-in idle admission for template replacement over the public gateway."""

import asyncio
from collections.abc import AsyncIterator, Callable

import pytest
import pytest_asyncio
import yaml

from wing_probe import (
    Driver,
    DriverHttpError,
    Probe,
    Session,
    ToolCall,
    Turn,
    assert_compact_transition,
)

OLD_MODEL = "probe/template-before"
NEW_MODEL = "probe/template-after"
UPDATE = "/api/session/update"
PREPARE_HOOK = """
import asyncio
from pathlib import Path

from wing.hooks import hooks

@hooks.on("before_user_message")
async def hold_input(content):
    if content == "hold preparation":
        Path("prepare-entered").touch()
        while not Path("prepare-release").exists():
            await asyncio.sleep(0.02)
"""
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
pytestmark = [
    pytest.mark.asyncio,
    pytest.mark.timeout(120),
    pytest.mark.probe_env(
        model=OLD_MODEL, models=[OLD_MODEL, NEW_MODEL], hooks=["hooks/*.py"]
    ),
]


@pytest_asyncio.fixture
async def templates(probe: Probe) -> AsyncIterator[Probe]:
    config_path = probe.env.config_path
    config = yaml.safe_load(config_path.read_text())
    config["agents"].append(
        {"name": "replacement", "model": NEW_MODEL, "tools": ["Read"]}
    )
    config_path.write_text(yaml.safe_dump(config))
    hooks = probe.env.root / "hooks"
    hooks.mkdir()
    (hooks / "prepare.py").write_text(PREPARE_HOOK)
    assert (await probe.driver_required.http.reload())["ok"] is True
    yield probe


async def _until(predicate: Callable[[], bool]) -> None:
    async with asyncio.timeout(8):
        while not predicate():
            await asyncio.sleep(0.02)


async def _replace(session: Session, **fields: object) -> dict:
    return await session.driver.http.request(
        "POST",
        UPDATE,
        body={
            "session_id": session.session_id,
            "agent": "replacement",
            "require_idle": True,
            **fields,
        },
    )


async def _expect_busy(session: Session) -> None:
    baseline = await session.info()
    before = session.history
    before_metadata = before.metadata()
    with pytest.raises(DriverHttpError) as rejected:
        await _replace(session)
    assert rejected.value.status == 409
    assert rejected.value.call.response["error"] == "error"
    assert "idle" in rejected.value.call.response["detail"]
    info = await session.info()
    for field in (
        "template_name",
        "model_id",
        "reasoning_effort",
        "thinking",
        "tools",
        "yolo",
    ):
        assert info[field] == baseline[field], field
    after = session.history
    assert after.records == before.records
    assert after.metadata() == before_metadata
    session.watch.assert_never("session_state_changed")
    session.watch.assert_never("interrupted")
    session.watch.assert_never("user_messages_dropped")


async def test_guarded_replacement_after_stale_idle_read(templates: Probe) -> None:
    probe = templates
    probe.register(
        OLD_MODEL, Turn.of(text="working finished"), Turn.of(text="queued finished")
    )
    probe.register(NEW_MODEL, Turn.of(text="new template finished"))
    session = await probe.session(effort="high", yolo=True, tools=["Glob"])
    external = await Driver.connect(probe.env)
    try:
        assert (await session.info())["status"] == "idle"
        with probe.provider.hold_first_content(OLD_MODEL) as reached:
            sent = await external.http.send_message(session.session_id, "external turn")
            await asyncio.wait_for(reached.wait(), timeout=8)
            assert (await session.info())["status"] == "working"
            queued = await session.send("queued input")
            await session.watch.expect(
                "user_message_queued", where={"origin_request_id": queued}
            )
            observer = await external.attach(session.session_id)
            snapshot = await observer.watch.expect("sync_session")
            assert snapshot.data["status"] == "working"
            assert [
                item["origin_request_id"]
                for item in snapshot.data["pending_user_messages"]
            ] == [queued]
            await _expect_busy(session)

        for answer in ("working finished", "queued finished"):
            assert (await session.watch.expect("turn_result")).data["result"] == answer
        assert [
            event.data["origin_request_ids"]
            for event in session.watch.events("user_message_committed")
        ] == [
            [sent["request_id"]],
            [queued],
        ]
        assert (await session.info())["status"] == "idle"
        before = session.history
        assert await _replace(session) == {"ok": True}
        changed = await session.watch.expect("session_state_changed")
        assert changed.data["agent"] == "replacement"
        assert changed.data["model_id"] == NEW_MODEL
        assert session.history.records == before.records
        info = await session.info()
        assert info["template_name"] == "replacement"
        assert info["tools"] == ["Read"]
        assert info["reasoning_effort"] is None
        assert info["yolo"] is False
        assert (await session.chat("after change")).data[
            "result"
        ] == "new template finished"
    finally:
        await external.close()


async def test_guarded_replacement_preserves_preparing_input(templates: Probe) -> None:
    probe = templates
    probe.register(OLD_MODEL, Turn.of(text="input finished"))
    session = await probe.session()
    request_id = await session.send("hold preparation")
    try:
        await _until((probe.env.root / "prepare-entered").exists)
        assert session.history.messages() == []
        assert probe.requests.count(OLD_MODEL) == 0
        await _expect_busy(session)
    finally:
        (probe.env.root / "prepare-release").touch()
    committed = await session.watch.expect("user_message_committed")
    assert committed.data["origin_request_ids"] == [request_id]
    assert (await session.watch.expect("turn_result")).data[
        "result"
    ] == "input finished"


async def test_guarded_replacement_preserves_waiting_ask(templates: Probe) -> None:
    probe = templates
    probe.register(OLD_MODEL, Turn.of(tool_calls=[ASK]), Turn.of(text="ask finished"))
    session = await probe.session(tools=["AskUserQuestion"])
    await session.send("ask before replacing")
    ask = await session.watch.expect("ask")
    assert (await session.info())["status"] == "waiting"
    await _expect_busy(session)
    await session.answer(ask, "red")
    assert (await session.watch.expect("turn_result")).data["result"] == "ask finished"
    probe.context(OLD_MODEL, 1).assert_tool_pairing()


async def test_guarded_replacement_preserves_manual_compaction(
    templates: Probe,
) -> None:
    probe = templates
    probe.register(
        OLD_MODEL,
        Turn.of(text="seed response"),
        Turn.of(text="<summary>Summary retained</summary>", first_delay=3),
    )
    session = await probe.session()
    assert (await session.chat("seed")).data["result"] == "seed response"
    before = session.history
    task = asyncio.create_task(session.compact())
    try:
        await _until(lambda: probe.requests.count(OLD_MODEL) == 2)
        assert (await session.info())["status"] == "idle"
        await _expect_busy(session)
    finally:
        result = await task
    assert result["ok"] is True
    await session.watch.expect("compact_done")
    assert_compact_transition(before, session.history)
    assert await _replace(session) == {"ok": True}


@pytest.mark.parametrize("guard", [{}, {"require_idle": False}])
async def test_legacy_template_update_can_still_interrupt(
    templates: Probe, guard: dict
) -> None:
    probe = templates
    probe.register(OLD_MODEL, Turn.of(text="response held"))
    probe.register(NEW_MODEL, Turn.of(text="replacement ready"))
    session = await probe.session()
    with probe.provider.hold_first_content(OLD_MODEL) as reached:
        await session.send("start")
        await asyncio.wait_for(reached.wait(), timeout=8)
        assert (await session.info())["status"] == "working"
        result = await session.driver.http.request(
            "POST",
            UPDATE,
            body={"session_id": session.session_id, "agent": "replacement", **guard},
        )
        assert result == {"ok": True}
        await session.watch.expect("interrupted")
    assert (await session.chat("after interrupt")).data["result"] == "replacement ready"


async def test_guarded_template_update_rejects_other_fields(templates: Probe) -> None:
    session = await templates.session()
    schema = await session.driver.http.request("GET", "/openapi.json")
    assert (
        schema["components"]["schemas"]["UpdateSessionRequest"]["properties"][
            "require_idle"
        ]["default"]
        is False
    )
    for fields in (
        {"agent": None},
        {"title": "wrong"},
        {"validate_reasoning": True},
        {"future_field": True},
    ):
        with pytest.raises(DriverHttpError) as invalid:
            await _replace(session, **fields)
        assert invalid.value.status == 400
    assert (await session.info())["template_name"] == "default"
