"""Settings conflicts describe only known paths and preserve the winner's bytes."""

from __future__ import annotations

import asyncio
import json
from copy import deepcopy

import pytest
import yaml

from wing_probe import DriverHttp, DriverHttpError, Probe


async def _save(http: DriverHttp, base: str | None, document: dict) -> dict:
    return await http.request(
        "POST", "/api/settings/set", body={"base": base, "document": document}
    )


@pytest.mark.asyncio
async def test_concurrent_writes_and_external_edits_report_paths(probe: Probe) -> None:
    http = probe.driver_required.http
    current = await http.request("GET", "/api/settings/get")
    base = current["fingerprint"]
    first, second = deepcopy(current["values"]), deepcopy(current["values"])
    first["steer"] = False
    second["yolo"] = True
    receipts = await asyncio.gather(
        _save(http, base, first), _save(http, base, second), return_exceptions=True
    )
    winner = next(result for result in receipts if isinstance(result, dict))
    loser = next(result for result in receipts if isinstance(result, DriverHttpError))
    assert winner["written"] is True
    assert loser.status == 409
    assert loser.call.response["error"] == "conflict"
    assert winner["fingerprint"] in loser.call.response["detail"]
    assert loser.call.response["conflict"] == {
        "current_fingerprint": winner["fingerprint"],
        "changed_paths": winner["changed"],
    }
    fresh = await http.request("GET", "/api/settings/get")
    assert fresh["fingerprint"] == winner["fingerprint"]

    path = probe.env.config_path
    disk = yaml.safe_load(path.read_bytes())
    disk["providers"][0]["api_key"] = "private-external-probe-8cfe"
    disk["log"] = {"level": "DEBUG"}
    path.write_text(yaml.safe_dump(disk), encoding="utf-8")
    external = path.read_bytes()
    with pytest.raises(DriverHttpError) as changed:
        await _save(http, fresh["fingerprint"], fresh["values"])
    assert changed.value.status == 409
    assert changed.value.call.response["conflict"]["changed_paths"] == [
        "providers[0].api_key",
        "log.level",
    ]
    assert "private-external-probe-8cfe" not in json.dumps(changed.value.call.response)
    assert path.read_bytes() == external

    with pytest.raises(DriverHttpError) as unknown:
        await _save(http, "0" * 64, fresh["values"])
    assert unknown.value.status == 409
    assert unknown.value.call.response["conflict"]["changed_paths"] is None
    assert path.read_bytes() == external

    updated = await http.request("GET", "/api/settings/get")
    assert (
        updated["fingerprint"]
        == unknown.value.call.response["conflict"]["current_fingerprint"]
    )
    path.write_bytes(external + b"# framing only\n")
    with pytest.raises(DriverHttpError) as comment:
        await _save(http, updated["fingerprint"], updated["values"])
    assert comment.value.status == 409
    assert comment.value.call.response["conflict"]["changed_paths"] == []

    updated = await http.request("GET", "/api/settings/get")
    assert (await _save(http, updated["fingerprint"], updated["values"]))[
        "written"
    ] is True
    assert (
        yaml.safe_load(path.read_bytes())["providers"][0]["api_key"]
        == "private-external-probe-8cfe"
    )
