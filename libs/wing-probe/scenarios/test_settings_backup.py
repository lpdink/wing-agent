"""保存前备份：``config.yaml.bak`` 是**改前文件的逐字副本**（覆盖式，只留最近一份）。

用户拿设置面板反复保存时，唯一能"回到上一份"的东西就是这个 ``.bak``（总设计 §7.3 ⑤
+ §20 风险 3：手写注释会被规范形重写）。它一旦不是逐字副本，回滚就会丢字段——
把 emitter 的输出或"合并后的文档"写进备份，都会让用户回滚到一个**自己没写过的**
配置。

覆盖的断言点：

- 首次保存：``backup_path`` 指向 ``<config>.bak``、``.bak`` 逐字节等于保存前的文件；
  主文件确实变了、仍是合法 YAML，且**单键 map 的 ``extra_body`` 原样往返**（AD7 第二处证据）；
- 第二次保存：``.bak`` 变成第一次保存之后的那份（覆盖式、只留最近一份），
  主文件与它的语义一致。
"""

from __future__ import annotations

import pytest
import yaml

from wing_probe import Probe

#: 透传键（单键 map——AD7 的最小复现形态）。
MARKER_KEY = "probe_backup_marker"
MARKER_VALUE = "one"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_backup_holds_the_previous_file_verbatim(probe: Probe) -> None:
    """两次保存：.bak 依次等于"每次保存前"的文件字节。"""
    http = probe.driver_required.http
    backup = probe.env.config_path.with_name("config.yaml.bak")

    # 基线：还没有 .bak（第 ⑤ 步是"存在才备份"）。
    assert probe.env.config_path.is_file()
    assert not backup.exists()

    # ── 第一次保存：单键 map 的 extra_body ──
    before_bytes = probe.env.config_path.read_bytes()
    current = await http.request("GET", "/api/settings/get")
    document = current["values"]
    document["providers"][0]["extra_body"] = {MARKER_KEY: MARKER_VALUE}

    first = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert first["ok"] is True, first
    assert first["backup_path"] == str(backup), first["backup_path"]
    assert backup.read_bytes() == before_bytes, (
        "backup must be the previous file verbatim"
    )

    after_first = probe.env.config_path.read_bytes()
    assert after_first != before_bytes
    written = yaml.safe_load(after_first.decode(encoding="utf-8"))
    assert written["providers"][0]["extra_body"] == {MARKER_KEY: MARKER_VALUE}, written[
        "providers"
    ][0]

    # ── 第二次保存：.bak 覆盖成"第一次之后"的那份 ──
    current = await http.request("GET", "/api/settings/get")
    document = current["values"]
    document["log"] = {"level": "DEBUG"}

    second = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert second["ok"] is True, second
    assert second["backup_path"] == str(backup), second["backup_path"]
    assert backup.read_bytes() == after_first, "the backup is overwritten, not appended"

    latest = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    assert latest["log"]["level"] == "DEBUG", latest["log"]
    assert latest["providers"][0]["extra_body"] == {MARKER_KEY: MARKER_VALUE}, latest[
        "providers"
    ][0]
