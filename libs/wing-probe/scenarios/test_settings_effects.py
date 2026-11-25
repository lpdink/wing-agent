"""Effective settings and save receipts through the real HTTP API."""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
from pathlib import Path

import pytest
import yaml

from wing_probe import (
    DriverHttp,
    DriverHttpError,
    Probe,
    Turn,
    reserve_port,
    resolve_gateway_bin,
    wait_for_health,
)


async def _get(http: DriverHttp) -> dict:
    return await http.request("GET", "/api/settings/get")


async def _save(http: DriverHttp, current: dict) -> dict:
    return await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": current["values"]},
    )


@pytest.mark.probe_env(provider_extra={"api_key": "probe-original-private-5cab"})
@pytest.mark.asyncio
async def test_effective_values_keep_defaults_and_secret_intent(probe: Probe) -> None:
    http = probe.driver_required.http
    current = await _get(http)
    assert "steer" not in current["values"]
    assert current["effective"]["values"]["steer"] is True
    assert current["effective"]["sources"]["steer"] == "default"
    assert current["effective"]["sources"]["providers[0].image_delivery"] == "default"
    assert current["effective"]["values"]["providers"][0]["api_key"] is None
    assert current["effective"]["sources"]["providers[0].api_key"] == "override"
    assert current["secrets"]["providers[0].api_key"] == {
        "state": "set",
        "hint": "5cab",
    }
    assert current["pending_restart"] == []
    assert "probe-original-private-5cab" not in json.dumps(current)

    provider = current["values"]["providers"][0]
    provider["image_delivery"] = None
    current["values"]["steer"] = True
    receipt = await _save(http, current)
    assert receipt["written"] is True
    assert "probe-original-private-5cab" not in json.dumps(receipt)
    current = await _get(http)
    assert current["effective"]["values"]["steer"] is True
    assert current["effective"]["sources"]["steer"] == "override"
    assert current["effective"]["values"]["providers"][0]["image_delivery"] is None
    assert current["effective"]["sources"]["providers[0].image_delivery"] == "override"
    assert yaml.safe_load(probe.env.config_path.read_bytes())["providers"][0][
        "api_key"
    ] == ("probe-original-private-5cab")


EFFECT_MODEL = "probe/settings-effects"
UNDISPATCHABLE_URL = "http://127.0.0.1:invalid/v1"


@pytest.mark.probe_env(
    models=[
        {
            "name": EFFECT_MODEL,
            "context_window_tokens": 64_000,
            "keep_recent_tokens": 10_000,
        }
    ]
)
@pytest.mark.asyncio
async def test_partial_reload_report_and_retry_preserve_the_committed_document(
    probe: Probe,
) -> None:
    http = probe.driver_required.http
    probe.register(EFFECT_MODEL, *(Turn.of(text="ok") for _ in range(4)))
    existing = await probe.session(model=EFFECT_MODEL)
    assert (await existing.chat("prime the provider pool")).data["subtype"] == "success"
    current = await _get(http)
    current["values"]["providers"][0]["base_url"] = UNDISPATCHABLE_URL
    current["values"]["providers"][0]["models"][0]["context_window_tokens"] = 96_000
    current["values"]["agents"][0]["system_prompt"] = "Only new sessions see this."
    current["values"]["log"] = {"level": "DEBUG"}
    receipt = await _save(http, current)
    assert receipt["ok"] is receipt["written"] is True
    assert receipt["problems"] == []
    assert receipt["reload"]["ok"] is False
    assert [
        step["name"] for step in receipt["reload"]["results"] if not step["ok"]
    ] == ["provider"]
    assert {effect["path"]: effect for effect in receipt["effects"]} == {
        "providers[0].base_url": {
            "path": "providers[0].base_url",
            "scope": "hot",
            "status": "failed",
            "steps": ["config.yaml", "provider"],
        },
        "providers[0].models[0].context_window_tokens": {
            "path": "providers[0].models[0].context_window_tokens",
            "scope": "next_session",
            "status": "next_session",
            "steps": ["config.yaml"],
        },
        "agents[0].system_prompt": {
            "path": "agents[0].system_prompt",
            "scope": "next_session",
            "status": "next_session",
            "steps": ["config.yaml"],
        },
        "log.level": {
            "path": "log.level",
            "scope": "hot",
            "status": "applied",
            "steps": ["config.yaml", "log level"],
        },
    }

    saved = await _get(http)
    assert saved["fingerprint"] == receipt["fingerprint"]
    assert (
        saved["effective"]["values"]["providers"][0]["base_url"] == UNDISPATCHABLE_URL
    )
    retry = await _save(http, saved)
    assert retry["written"] is True
    assert retry["fingerprint"] == receipt["fingerprint"]
    assert retry["effects"] == retry["changed"] == []
    assert retry["reload"]["ok"] is False
    assert (await existing.chat("old pool still handles the request")).data[
        "subtype"
    ] == "success"
    assert (
        probe.context(EFFECT_MODEL, index=1).system
        == probe.context(EFFECT_MODEL, index=0).system
    )

    new = await probe.session(model=EFFECT_MODEL)
    for session, window in ((existing, 64_000), (new, 96_000)):
        info = await http.request(
            "GET", "/api/session/info", params={"session_id": session.session_id}
        )
        assert info["context_window_tokens"] == window

    saved["values"]["providers"][0]["base_url"] = probe.env.provider.base_url
    repaired = await _save(http, saved)
    assert repaired["reload"]["ok"] is True
    assert repaired["effects"][0]["status"] == "applied"
    assert (await existing.chat("old template remains")).data["subtype"] == "success"
    assert (await new.chat("new template")).data["subtype"] == "success"
    assert (
        probe.context(EFFECT_MODEL, index=2).system
        == probe.context(EFFECT_MODEL, index=0).system
    )
    assert probe.context(EFFECT_MODEL, index=3).system == "Only new sessions see this."


USER_AGENT_MODEL = "probe/settings-user-agent"


@pytest.mark.probe_env(models=[USER_AGENT_MODEL])
@pytest.mark.asyncio
async def test_user_agent_effect_tracks_provider_rebuild_and_actual_headers(
    probe: Probe,
) -> None:
    http = probe.driver_required.http
    probe.register(
        USER_AGENT_MODEL,
        Turn.of(text="before"),
        Turn.of(text="failed rebuild"),
        Turn.of(text="repaired"),
    )
    session = await probe.session(model=USER_AGENT_MODEL)
    assert (await session.chat("prime the provider pool")).data["subtype"] == "success"
    before = probe.request(USER_AGENT_MODEL, 0).headers
    assert before["user-agent"].startswith("QwenCode/")
    assert before["x-dashscope-useragent"] == before["user-agent"]

    current = await _get(http)
    current["values"]["user_agent"] = {"preset": "opencode"}
    current["values"]["providers"][0]["base_url"] = UNDISPATCHABLE_URL
    failed = await _save(http, current)
    assert failed["written"] is True
    assert failed["reload"]["ok"] is False
    assert [step["name"] for step in failed["reload"]["results"] if not step["ok"]] == [
        "provider"
    ]
    assert next(
        effect for effect in failed["effects"] if effect["path"] == "user_agent.preset"
    ) == {
        "path": "user_agent.preset",
        "scope": "hot",
        "status": "failed",
        "steps": ["config.yaml", "provider"],
    }
    assert (await session.chat("use the existing provider")).data[
        "subtype"
    ] == "success"
    after_failure = probe.request(USER_AGENT_MODEL, 1).headers
    assert after_failure["user-agent"] == before["user-agent"]
    assert after_failure["x-dashscope-useragent"] == before["x-dashscope-useragent"]

    for _ in range(2):
        current = await _get(http)
        assert current["fingerprint"] == failed["fingerprint"]
        assert current["effective"]["values"]["user_agent"]["preset"] == "opencode"
        retry = await _save(http, current)
        assert retry["written"] is True
        assert retry["fingerprint"] == failed["fingerprint"]
        assert retry["changed"] == []
        assert [
            step["name"] for step in retry["reload"]["results"] if not step["ok"]
        ] == ["provider"]
    manual = await http.request("POST", "/api/system/reload")
    assert manual["ok"] is False
    assert [step["name"] for step in manual["results"] if not step["ok"]] == [
        "provider"
    ]

    current = await _get(http)
    current["values"]["providers"][0]["base_url"] = probe.env.provider.base_url
    repaired = await _save(http, current)
    assert repaired["reload"]["ok"] is True
    assert repaired["reload"]["failed_settings"] == []
    assert (await session.chat("use the rebuilt provider")).data["subtype"] == "success"
    after_repair = probe.request(USER_AGENT_MODEL, 2).headers
    assert after_repair["user-agent"].startswith("opencode/")
    assert "x-dashscope-useragent" not in after_repair


RESTART_ORIGIN = "https://settings.probe.example"


@pytest.mark.asyncio
async def test_pending_restart_survives_another_save_and_clears_on_restart(
    probe: Probe,
) -> None:
    http = probe.driver_required.http
    current = await _get(http)
    current["values"]["gateway"]["cors"] = {"origins": [RESTART_ORIGIN]}
    receipt = await _save(http, current)
    restart_path = "gateway.cors.origins[0]"
    assert receipt["pending_restart"] == receipt["restart_required"] == [restart_path]
    assert receipt["effects"] == [
        {
            "path": restart_path,
            "scope": "restart",
            "status": "restart_required",
            "steps": [],
        }
    ]

    current = await _get(http)
    assert current["pending_restart"] == [restart_path]
    receipt = await _save(http, current)
    assert receipt["effects"] == receipt["restart_required"] == []
    assert receipt["pending_restart"] == [restart_path]
    preflight_headers = {
        "Origin": RESTART_ORIGIN,
        "Access-Control-Request-Method": "GET",
    }
    with pytest.raises(DriverHttpError) as forbidden:
        await http.request_raw(
            "OPTIONS", "/api/settings/get", headers=preflight_headers
        )
    assert forbidden.value.call.status == 400

    current = await _get(http)
    current["values"]["gateway"].pop("cors")
    reverted = await _save(http, current)
    assert reverted["pending_restart"] == []
    assert reverted["effects"][0]["status"] == "applied"

    current = await _get(http)
    current["values"]["gateway"]["cors"] = {"origins": [RESTART_ORIGIN]}
    receipt = await _save(http, current)
    assert receipt["pending_restart"] == [restart_path]
    probe.env.config_text = probe.env.config_path.read_text(encoding="utf-8")
    await probe.restart_gateway()
    http = probe.driver_required.http
    current = await _get(http)
    assert current["fingerprint"] == receipt["fingerprint"]
    assert current["pending_restart"] == []
    call, _ = await http.request_raw(
        "OPTIONS", "/api/settings/get", headers=preflight_headers
    )
    assert call.response_headers["access-control-allow-origin"] == RESTART_ORIGIN


@pytest.mark.asyncio
async def test_unknown_removed_provider_key_cannot_become_a_gateway_setting(
    probe: Probe,
) -> None:
    http = probe.driver_required.http
    path = probe.env.config_path
    raw = yaml.safe_load(path.read_bytes())
    old_port = raw["gateway"]["port"]
    unknown = {"port": old_port + 1, "future_setting": True}
    raw["providers"].append(
        {
            **raw["providers"][0],
            "name": "extra",
            "models": ["probe/extra"],
            "gateway": unknown,
        }
    )
    raw["_wing_orphaned_settings"] = "existing unknown root key"
    path.write_text(yaml.safe_dump(raw), encoding="utf-8")
    current = await _get(http)
    current["values"]["providers"].pop()
    saved = await _save(http, current)
    assert saved["written"] is True
    assert saved["reload"]["ok"] is True
    assert saved["pending_restart"] == saved["restart_required"] == []
    assert "gateway.port" not in saved["changed"]
    current = await _get(http)
    assert current["fingerprint"] == saved["fingerprint"]
    assert current["pending_restart"] == saved["pending_restart"]
    assert current["effective"]["values"]["gateway"]["port"] == old_port
    disk = yaml.safe_load(path.read_bytes())
    assert disk["_wing_orphaned_settings"] == "existing unknown root key"
    assert disk["_wing_orphaned_settings_"] == [
        {"parent": "providers[1]", "key": "gateway", "value": unknown}
    ]


@pytest.mark.asyncio
async def test_file_loading_reports_failed_patterns_and_keeps_successful_commands(
    probe: Probe,
) -> None:
    http = probe.driver_required.http
    current = await _get(http)
    root = probe.env.root
    (root / "good.md").write_text(
        "---\nname: probe-good\n---\nRun a probe command.\n", encoding="utf-8"
    )
    (root / "incomplete.md").write_text(
        "---\ndescription: missing name\n---\nDemo text.\n", encoding="utf-8"
    )
    (root / "bad_hook.py").write_text("def\n", encoding="utf-8")
    current["values"]["commands"] = {
        "paths": ["good.md", "incomplete.md", "missing-*.md"]
    }
    current["values"]["hooks"] = ["bad_hook.py"]
    saved = await _save(http, current)
    assert saved["written"] is True
    assert {effect["path"]: effect["status"] for effect in saved["effects"]} == {
        "hooks[0]": "failed",
        "commands.paths[0]": "applied",
        "commands.paths[1]": "failed",
        "commands.paths[2]": "applied",
    }
    commands = await http.request("GET", "/api/commands")
    assert [
        command["name"]
        for command in commands["commands"]
        if command["name"].startswith("probe-")
    ] == ["probe-good"]
    failed_paths = ["commands.paths[1]", "hooks[0]"]
    assert saved["reload"]["failed_settings"] == failed_paths
    assert saved["reload"]["ok"] is True
    for _ in range(2):
        current = await _get(http)
        assert current["fingerprint"] == saved["fingerprint"]
        retry = await _save(http, current)
        assert retry["written"] is True
        assert retry["fingerprint"] == saved["fingerprint"]
        assert retry["changed"] == retry["effects"] == []
        assert retry["reload"]["failed_settings"] == failed_paths
        assert retry["reload"]["ok"] is True
        assert all(step["ok"] for step in retry["reload"]["results"])
        details = {step["name"]: step["detail"] for step in retry["reload"]["results"]}
        assert "hooks[0]" in details["hooks"]
        assert "commands.paths[1]" in details["prompt commands"]

    manual = await http.request("POST", "/api/system/reload")
    assert manual["ok"] is True
    assert manual["failed_settings"] == failed_paths
    assert [step["name"] for step in manual["results"]] == [
        step["name"] for step in saved["reload"]["results"]
    ]
    assert all(step["ok"] for step in manual["results"])

    (root / "incomplete.md").write_text(
        "---\nname: probe-repaired\n---\nRepaired command.\n", encoding="utf-8"
    )
    (root / "bad_hook.py").write_text("# Loadable hook module.\n", encoding="utf-8")
    retried = await _save(http, await _get(http))
    assert retried["fingerprint"] == saved["fingerprint"]
    assert retried["changed"] == retried["effects"] == []
    assert retried["reload"]["failed_settings"] == []
    assert retried["reload"]["ok"] is True
    manual = await http.request("POST", "/api/system/reload")
    assert manual["failed_settings"] == []
    assert manual["ok"] is True
    commands = await http.request("GET", "/api/commands")
    assert {
        command["name"]
        for command in commands["commands"]
        if command["name"].startswith("probe-")
    } == {"probe-good", "probe-repaired"}


@pytest.mark.asyncio
async def test_quoted_port_uses_the_validated_boot_value(
    probe: Probe, tmp_path: Path
) -> None:
    home = tmp_path / "quoted-port"
    (home / "core").mkdir(parents=True)
    port = reserve_port()
    raw = yaml.safe_load(probe.env.config_path.read_bytes())
    raw["gateway"]["port"] = str(port)
    config = home / "core" / "config.yaml"
    config.write_text(yaml.safe_dump(raw), encoding="utf-8")
    home_env = {
        **os.environ,
        "WING_HOME": str(home),
        "WING_SESSIONS_PATH": str(home / "sessions"),
    }
    url = f"http://127.0.0.1:{port}"
    http = DriverHttp(url)
    log = home / "gateway.log"
    with log.open("wb") as output:
        process = subprocess.Popen(
            [str(resolve_gateway_bin())],
            cwd=home,
            env=home_env,
            stdout=output,
            stderr=subprocess.STDOUT,
        )
        try:
            await wait_for_health(
                f"{url}/api/health", timeout=10.0, process=process, log_path=log
            )
            current = await _get(http)
            assert current["config_path"] == str(config)
            assert current["values"]["gateway"]["port"] == str(port)
            assert current["effective"]["values"]["gateway"]["port"] == port
            assert current["pending_restart"] == []
            current["values"]["gateway"]["port"] = reserve_port()
            assert current["values"]["gateway"]["port"] != port
            assert (await _save(http, current))["pending_restart"] == ["gateway.port"]
            current = await _get(http)
            current["values"]["gateway"]["port"] = port
            assert (await _save(http, current))["pending_restart"] == []
        finally:
            await http.close()
            if process.poll() is None:
                process.terminate()
            try:
                await asyncio.to_thread(process.wait, 5)
            except subprocess.TimeoutExpired:
                process.kill()
                await asyncio.to_thread(process.wait, 5)
