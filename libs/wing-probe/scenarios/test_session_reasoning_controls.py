"""Session reasoning readback and edits against both upstream request protocols."""

from __future__ import annotations

import pytest
import yaml

from wing_probe import Driver, DriverHttpError, Probe, Turn

from _session_helpers import evict

OPENAI = "probe/reasoning-openai"
ANTHROPIC = "probe/reasoning-anthropic"
UNDECLARED = "probe/reasoning-undeclared"
OPENAI_MODEL = {
    "name": OPENAI,
    "capabilities": {"thinking": True, "reasoning_effort": ["low", "xhigh"]},
}
ANTHROPIC_MODEL = {
    "name": ANTHROPIC,
    "capabilities": {"thinking": True, "reasoning_effort": ["high"]},
}
UPDATE = "/api/session/update"

pytestmark = [pytest.mark.asyncio, pytest.mark.timeout(120)]


@pytest.mark.probe_env(
    models=[OPENAI_MODEL, UNDECLARED],
    provider_extra={"extra_body": {"reasoning_effort": "low"}},
)
async def test_openai_options_follow_model_and_persist_after_resume(
    probe: Probe,
) -> None:
    probe.register(OPENAI, *(Turn.of(text="OK") for _ in range(3)))
    probe.register(UNDECLARED, Turn.of(text="legacy OK"))
    session = await probe.session(model=OPENAI)
    sid = session.session_id
    http = probe.driver_required.http
    strict = {"session_id": sid, "validate_reasoning": True}
    default = {
        "thinking": True,
        "thinking_supported": True,
        "effort": "low",
        "effort_options": ["low", "xhigh"],
    }
    assert (await session.info())["reasoning"] == default
    await session.chat("check configured default")
    first = probe.request(OPENAI, 0).body
    assert first["enable_thinking"] is True
    assert first["reasoning_effort"] == "low"

    metadata = probe.history(session).metadata()
    with pytest.raises(DriverHttpError) as rejected:
        await http.request(
            "POST",
            UPDATE,
            body={**strict, "thinking": False, "reasoning_effort": "high"},
        )
    assert rejected.value.status == 400
    assert "low, xhigh" in rejected.value.call.response["detail"]
    assert (await session.info())["reasoning"] == default
    assert probe.history(session).metadata() == metadata

    external = await Driver.connect(probe.env)
    try:
        assert await external.http.request(
            "POST",
            UPDATE,
            body={**strict, "thinking": False, "reasoning_effort": "xhigh"},
        ) == {"ok": True}
        changed = await session.watch.expect("session_state_changed")
        assert changed.data["session_id"] == sid
        assert changed.data["thinking"] is False
        assert changed.data["reasoning_effort"] == "xhigh"
    finally:
        await external.close()

    edited = {**default, "thinking": False, "effort": "xhigh"}
    assert (await session.info())["reasoning"] == edited
    await session.chat("check edited flags")
    edited_body = probe.request(OPENAI, 1).body
    assert edited_body["enable_thinking"] is False
    assert edited_body["reasoning_effort"] == "xhigh"

    await evict(probe, sid)
    with pytest.raises(DriverHttpError) as unloaded:
        await session.info()
    assert unloaded.value.status == 404
    session = await probe.resume(sid)
    await http.subscribe(sid, probe.driver_required.client_id)
    assert (await session.info())["reasoning"] == edited
    await session.chat("check restored flags")
    restored = probe.request(OPENAI, 2).body
    assert restored["enable_thinking"] is False
    assert restored["reasoning_effort"] == "xhigh"

    await http.update_session(sid, model_id=UNDECLARED)
    assert (await session.info())["reasoning"] == {
        **edited,
        "thinking_supported": None,
        "effort_options": None,
    }
    for field, value in (("thinking", True), ("reasoning_effort", "low")):
        with pytest.raises(DriverHttpError) as unknown:
            await http.request("POST", UPDATE, body={**strict, field: value})
        assert unknown.value.status == 400
    await http.update_session(sid, reasoning_effort="high")
    await session.chat("check legacy controls on undeclared model")
    assert probe.request(UNDECLARED).body["reasoning_effort"] == "high"


@pytest.mark.probe_env(
    models=[OPENAI_MODEL],
    extra_providers=[{"name": "anthropic", "models": [ANTHROPIC_MODEL]}],
    provider_extra={"reasoning_effort": "low"},
)
async def test_anthropic_controls_follow_protocol_and_template_reset(
    probe: Probe,
) -> None:
    config_path = probe.env.wing_home / "core" / "config.yaml"
    config = yaml.safe_load(config_path.read_text())
    config["providers"][1].update(
        protocol="anthropic",
        base_url=probe.env.provider.url,
        extra_body={"thinking": {"type": "disabled", "budget_tokens": 2048}},
    )
    config["agents"].append({"name": "claude", "model": ANTHROPIC, "tools": ["Read"]})
    config_path.write_text(yaml.safe_dump(config))
    http = probe.driver_required.http
    assert (await http.reload())["ok"] is True

    probe.register(OPENAI, Turn.of(text="OpenAI OK"))
    probe.register(ANTHROPIC, *(Turn.of(text="Anthropic OK") for _ in range(4)))
    session = await probe.session(
        template="default", model=OPENAI, tools=["Glob"], yolo=True
    )
    sid = session.session_id
    strict = {"session_id": sid, "validate_reasoning": True}
    assert (await session.info())["template_name"] == "default"
    await http.request(
        "POST", UPDATE, body={**strict, "thinking": True, "reasoning_effort": "xhigh"}
    )
    await session.chat("materialize the overrides")

    await http.update_session(sid, model_id=ANTHROPIC)
    enabled = {
        "thinking": True,
        "thinking_supported": True,
        "effort": None,
        "effort_options": [],
    }
    info = await session.info()
    assert info["provider_name"] == "anthropic"
    assert info["reasoning_effort"] == "xhigh"
    assert info["reasoning"] == enabled
    await session.chat("retained thinking across providers")
    first = probe.request(ANTHROPIC, 0)
    assert first.path == "/v1/messages"
    assert first.body["thinking"] == {"type": "enabled", "budget_tokens": 2048}
    assert "reasoning_effort" not in first.body
    assert first.headers["anthropic-beta"] == "interleaved-thinking-2025-05-14"

    with pytest.raises(DriverHttpError) as unsupported:
        await http.request(
            "POST",
            UPDATE,
            body={**strict, "thinking": False, "reasoning_effort": "high"},
        )
    assert unsupported.value.status == 400
    assert (await session.info())["reasoning"] == enabled
    await http.request("POST", UPDATE, body={**strict, "thinking": True})
    await session.chat("strict thinking reaches Anthropic")
    assert probe.request(ANTHROPIC, 1).body["thinking"] == first.body["thinking"]

    workspace = probe.env.root / "other-workspace"
    workspace.mkdir()
    await http.update_session(sid, workspace=str(workspace))
    history = probe.history(session).messages()
    with pytest.raises(DriverHttpError) as mixed:
        await http.request(
            "POST", UPDATE, body={**strict, "agent": "claude", "thinking": False}
        )
    assert mixed.value.status == 400
    assert (await session.info())["template_name"] == "default"
    await http.update_session(sid, agent="claude")
    reset = {**enabled, "thinking": False}
    info = await session.info()
    assert info["template_name"] == "claude"
    assert info["model_id"] == ANTHROPIC
    assert info["reasoning"] == reset
    assert info["tools"] == ["Read"]
    assert info["yolo"] is False
    assert info["workdir"] == str(workspace.resolve())
    assert probe.history(session).messages() == history
    metadata = probe.history(session).metadata()
    assert metadata is not None
    assert all(
        key not in metadata for key in ("thinking", "reasoning_effort", "yolo", "tools")
    )
    await session.chat("check request after template reset")
    after = probe.request(ANTHROPIC, 2)
    assert after.body["thinking"] == {"type": "disabled"}
    assert "anthropic-beta" not in after.headers
    assert "reasoning_effort" not in after.body

    await http.request("POST", UPDATE, body={**strict, "thinking": True})
    await evict(probe, sid)
    session = await probe.resume(sid)
    await http.subscribe(sid, probe.driver_required.client_id)
    info = await session.info()
    assert info["template_name"] == "claude"
    assert info["reasoning"] == enabled
    await session.chat("check restored Anthropic override")
    assert probe.request(ANTHROPIC, 3).body["thinking"] == first.body["thinking"]


@pytest.mark.probe_env(
    models=[ANTHROPIC_MODEL],
    provider_extra={
        "protocol": "anthropic",
        "extra_body": {"thinking": {"type": "adaptive"}},
    },
)
async def test_adaptive_thinking_reads_as_enabled_and_refreshes_on_reload(
    probe: Probe,
) -> None:
    probe.register(ANTHROPIC, Turn.of(text="adaptive OK"), Turn.of(text="standard OK"))
    session = await probe.session(model=ANTHROPIC)
    http = probe.driver_required.http
    strict = {
        "session_id": session.session_id,
        "validate_reasoning": True,
        "thinking": False,
    }
    adaptive = {
        "thinking": True,
        "thinking_supported": False,
        "effort": None,
        "effort_options": [],
    }
    assert (await session.info())["reasoning"] == adaptive
    with pytest.raises(DriverHttpError) as unsupported:
        await http.request("POST", UPDATE, body=strict)
    assert unsupported.value.status == 400
    await session.chat("check adaptive request")
    assert probe.request(ANTHROPIC, 0).body["thinking"] == {"type": "adaptive"}

    config_path = probe.env.wing_home / "core" / "config.yaml"
    config = yaml.safe_load(config_path.read_text())
    config["providers"][0]["extra_body"]["thinking"] = {
        "type": "enabled",
        "budget_tokens": 1024,
    }
    config_path.write_text(yaml.safe_dump(config))
    assert (await http.reload())["ok"] is True
    standard = {**adaptive, "thinking_supported": True}
    assert (await session.info())["reasoning"] == standard
    await http.request("POST", UPDATE, body=strict)
    assert (await session.info())["reasoning"] == {**standard, "thinking": False}
    await session.chat("check standard thinking disabled")
    assert probe.request(ANTHROPIC, 1).body["thinking"] == {"type": "disabled"}
