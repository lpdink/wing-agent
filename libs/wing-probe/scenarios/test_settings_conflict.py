"""乐观并发：外部改文件后旧指纹保存被拒（409），换新指纹成功。

两个客户端同时编辑时后端不做三方合并（总设计 §20 风险 6）：客户端保存时必须带上
它读到的**指纹**，盘上文件变了就 409 并**一个字节都不写**。指纹是
``sha256(file bytes)``——外部编辑器追加一行注释也会让它变。

覆盖的断言点：

- 旧指纹 save → **HTTP 409**、body ``error == "conflict"``、``detail`` 里带**当前**指纹
  （客户端据此重新 `get`，不是死路）；
- 冲突时文件**逐字节未被覆盖**（外部编辑原样还在）；
- 重新取指纹后用同一份文档保存 → ``ok=true`` 且 ``changed == []``
  （文档与盘上语义一致，diff 为空）；
- 这次成功保存的 ``.bak`` = **改前的文件（含外部编辑）**逐字——备份是"保存前的磁盘事实"；
- 重写后的文件仍是合法 YAML，且语义与保存前一致。
"""

from __future__ import annotations

import pytest
import yaml

from wing_probe import DriverHttpError, Probe

#: 外部编辑追加的注释行（指纹会因此变化，但文件仍合法）。
EXTERNAL_EDIT = "# edited outside the gateway (probe: stale fingerprint)"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_stale_base_conflicts_and_the_file_survives(probe: Probe) -> None:
    """旧指纹 409 + 文件未被覆盖；新指纹成功 + .bak 是改前文件。"""
    http = probe.driver_required.http

    current = await http.request("GET", "/api/settings/get")
    stale_fingerprint = current["fingerprint"]

    # 外部改文件：追加一行注释（保持 YAML 合法——网关进程不受影响）。
    text = probe.env.config_path.read_text(encoding="utf-8")
    probe.env.config_path.write_text(f"{text}{EXTERNAL_EDIT}\n", encoding="utf-8")
    edited_bytes = probe.env.config_path.read_bytes()

    # ── 旧指纹：409，且文件未被触碰 ──
    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "POST",
            "/api/settings/set",
            body={"base": stale_fingerprint, "document": current["values"]},
        )
    call = failure.value.call
    assert call.status == 409, call.render()
    assert call.response["error"] == "conflict", call.response
    assert probe.env.config_path.read_bytes() == edited_bytes

    # 冲突响应里的指纹就是**当前**指纹（客户端据此重取，不是死路）。
    fresh = await http.request("GET", "/api/settings/get")
    assert fresh["fingerprint"] != stale_fingerprint, fresh
    assert fresh["fingerprint"] in call.response["detail"], call.response

    # ── 新指纹：成功，且文档与盘上语义一致（changed 为空） ──
    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": fresh["fingerprint"], "document": fresh["values"]},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["problems"] == [], receipt
    assert receipt["changed"] == [], receipt
    assert receipt["restart_required"] == [], receipt
    assert receipt["reload"]["ok"] is True, receipt["reload"]

    # .bak = 保存前那一刻的磁盘事实（含外部编辑的那一行）。
    backup = probe.env.config_path.with_name("config.yaml.bak")
    assert receipt["backup_path"] == str(backup), receipt
    assert backup.read_bytes() == edited_bytes, backup.read_text(encoding="utf-8")[
        -300:
    ]

    # 重写后的文件是规范形（合法 YAML，语义不变）。注意 values 里 api_key 是掩码后的
    # null（"保留磁盘现值"的三态），所以密文按"盘上仍是真值"单独断言。
    rewritten = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    provider = rewritten["providers"][0]
    assert provider["name"] == fresh["values"]["providers"][0]["name"], provider
    assert provider["base_url"] == fresh["values"]["providers"][0]["base_url"], provider
    assert isinstance(provider["api_key"], str) and provider["api_key"], provider
    assert rewritten["agents"] == fresh["values"]["agents"], rewritten["agents"]

    # 新文件指纹与回执一致（下一次保存的基线）。
    again = await http.request("GET", "/api/settings/get")
    assert again["fingerprint"] == receipt["fingerprint"], (again, receipt)
