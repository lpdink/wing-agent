"""设置保存的 409 冲突恢复与逐字节备份。

外部编辑后的旧指纹保存应保留主文件原字节。恢复保存和后续每次保存都应将写入前的
磁盘字节放入 ``config.yaml.bak``。这让用户能恢复手写注释和上一版配置，即使保存
把主文件重写成了规范形。
"""

from __future__ import annotations

import pytest
import yaml

from wing_probe import DriverHttp, DriverHttpError, Probe

EXTERNAL_EDIT = "# edited outside the gateway (probe: stale fingerprint)"


async def _save(http: DriverHttp, current: dict) -> dict:
    return await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": current["values"]},
    )


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_conflict_recovery_and_repeated_saves_back_up_previous_bytes(
    probe: Probe,
) -> None:
    http = probe.driver_required.http
    path = probe.env.config_path
    backup = path.with_name("config.yaml.bak")
    assert path.is_file()
    assert not backup.exists()

    current = await http.request("GET", "/api/settings/get")
    stale_fingerprint = current["fingerprint"]

    # 外部改文件：追加一行注释（保持 YAML 合法——网关进程不受影响）。
    text = path.read_text(encoding="utf-8")
    path.write_text(f"{text}{EXTERNAL_EDIT}\n", encoding="utf-8")
    edited_bytes = path.read_bytes()

    # ── 旧指纹：409，且文件未被触碰 ──
    with pytest.raises(DriverHttpError) as failure:
        await _save(http, current)
    call = failure.value.call
    assert call.status == 409, call.render()
    assert call.response["error"] == "conflict", call.response
    assert path.read_bytes() == edited_bytes

    # 冲突响应里的指纹就是**当前**指纹（客户端据此重取，不是死路）。
    fresh = await http.request("GET", "/api/settings/get")
    assert fresh["fingerprint"] != stale_fingerprint, fresh
    assert fresh["fingerprint"] in call.response["detail"], call.response

    # ── 新指纹：成功，且文档与盘上语义一致（changed 为空） ──
    receipt = await _save(http, fresh)
    assert receipt["ok"] is True, receipt
    assert receipt["problems"] == [], receipt
    assert receipt["changed"] == [], receipt
    assert receipt["restart_required"] == [], receipt
    assert receipt["reload"]["ok"] is True, receipt["reload"]

    # .bak = 保存前那一刻的磁盘事实（含外部编辑的那一行）。
    assert receipt["backup_path"] == str(backup), receipt
    assert backup.read_bytes() == edited_bytes, (
        "recovery backup must include the external edit"
    )

    # 重写后的文件是规范形（合法 YAML，语义不变）。注意 values 里 api_key 是掩码后的
    # null（"保留磁盘现值"的三态），所以密文按"盘上仍是真值"单独断言。
    recovered_bytes = path.read_bytes()
    rewritten = yaml.safe_load(recovered_bytes)
    provider = rewritten["providers"][0]
    assert provider["name"] == fresh["values"]["providers"][0]["name"], provider
    assert provider["base_url"] == fresh["values"]["providers"][0]["base_url"], provider
    assert isinstance(provider["api_key"], str) and provider["api_key"], provider
    assert rewritten["agents"] == fresh["values"]["agents"], rewritten["agents"]

    # 新文件指纹与回执一致（下一次保存的基线）。
    again = await http.request("GET", "/api/settings/get")
    assert again["fingerprint"] == receipt["fingerprint"], (again, receipt)

    marker = {"probe_backup_marker": "one"}
    again["values"]["providers"][0]["extra_body"] = marker
    first = await _save(http, again)
    assert first["ok"] is True, first
    assert first["backup_path"] == str(backup), first
    assert backup.read_bytes() == recovered_bytes, "backup before saving extra_body"
    after_first = path.read_bytes()
    assert after_first != recovered_bytes
    assert yaml.safe_load(after_first)["providers"][0]["extra_body"] == marker

    current = await http.request("GET", "/api/settings/get")
    assert current["fingerprint"] == first["fingerprint"], (current, first)
    current["values"]["log"] = {"level": "DEBUG"}
    second = await _save(http, current)
    assert second["ok"] is True, second
    assert second["backup_path"] == str(backup), second
    assert backup.read_bytes() == after_first, "backup before saving log.level"
    latest = yaml.safe_load(path.read_bytes())
    assert latest["log"]["level"] == "DEBUG", latest["log"]
    assert latest["providers"][0]["extra_body"] == marker, latest["providers"][0]
