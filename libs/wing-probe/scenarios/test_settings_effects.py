"""Effective settings and save receipts through the real HTTP API."""

from __future__ import annotations

import json

import pytest
import yaml

from wing_probe import DriverHttp, DriverHttpError, Probe, Turn


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


@pytest.mark.probe_env(models=[EFFECT_MODEL])
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

    saved["values"]["providers"][0]["base_url"] = probe.env.provider.base_url
    repaired = await _save(http, saved)
    assert repaired["reload"]["ok"] is True
    assert repaired["effects"][0]["status"] == "applied"
    assert (await existing.chat("old template remains")).data["subtype"] == "success"
    new = await probe.session(model=EFFECT_MODEL)
    assert (await new.chat("new template")).data["subtype"] == "success"
    assert (
        probe.context(EFFECT_MODEL, index=2).system
        == probe.context(EFFECT_MODEL, index=0).system
    )
    assert probe.context(EFFECT_MODEL, index=3).system == "Only new sessions see this."


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
