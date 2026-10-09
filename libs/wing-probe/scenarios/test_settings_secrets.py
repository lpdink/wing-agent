"""密文（api_key）的只写不回显：get 搜不到真值 / hint 是末 4 位 / `null` = 保留。

密文语义（总设计 §7.5）的契约红线：**前端必须原样回传 `null`**——`null` = 保留磁盘
现值，丢键 = 从文件移除（必填字段随即成为 problem）。保存路径上这个分支写错
（例如把 `null` 当成"空串"或"删除键"）不会有任何直观报错：用户下次调用才 401。

覆盖的断言点：

- `get`：`values.providers[0].api_key is None`（掩码），**原始响应文本里搜不到真实 key**，
  `secrets` 表给出 `{"state": "set", "hint": <末 4 位>}`；
- 嵌套密文同样受控：文档里加一条 `gateway.auth.keys[]`（`enabled` 保持 false）后
  `gateway.auth.keys[0].key` 也进 `secrets` 表、真值同样不出现在响应里；
- 字符串覆盖生效：改成新 key → 响应体搜不到新 key、盘上文件是真值、会话照常收尾；
- **`null` 保留原值**：再 `get`（key 是 `null`）→ 只改另一个字段 → 盘上仍是新 key
  （不是 null / 空串），会话照常收尾（假 Provider 不校验凭据，见文件末的"断言强度"）。
"""

from __future__ import annotations

from typing import Any

import pytest
import yaml

from wing_probe import Probe, Turn

#: 场景私有 model 名。
SECRETS_MODEL = "probe/settings-secrets"

#: env 生成的初始 provider key（``DEFAULT_PROVIDER_API_KEY``）。
INITIAL_KEY = "probe-key"
#: 覆盖用的新 key（末 4 位 = "API7"，与初始 key 的 hint 可区分）。
NEW_KEY = "probe-secret-API7"
#: 嵌套密文（``gateway.auth.keys[0].key``）。
AUTH_KEY = "gateway-gate-9Z1K"


async def _settings(http: Any) -> dict:
    return await http.request("GET", "/api/settings/get")


def _response_text(probe: Probe, path: str) -> str:
    """最近一次某路径的**原始响应文本**（截断前的 wire 事实；搜"没有泄露"用）。"""
    call = probe.last_http_call(path=path)
    assert call is not None, f"no recorded call for {path}"
    return call.text


def _assert_not_echoed(probe: Probe, path: str, secret: str) -> None:
    text = _response_text(probe, path)
    assert secret not in text, f"{secret!r} leaked in {path} response: {text}"
    # 密文值也可能被序列化进任何 JSON 字段（如 problems 的 hint）——搜响应对象本身。
    call = probe.last_http_call(path=path)
    assert secret not in str(call.response), (path, call.response)


@pytest.mark.probe_env(models=[SECRETS_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_secrets_are_never_echoed_and_null_keeps_the_value(probe: Probe) -> None:
    """get 不回显真值；set 的 `null` 保留磁盘现值、字符串覆盖生效。"""
    probe.register(
        SECRETS_MODEL, Turn.of(text="r1"), Turn.of(text="r2"), Turn.of(text="r3")
    )
    session = await probe.session(model=SECRETS_MODEL)
    http = probe.driver_required.http

    first = await session.chat("baseline")
    assert first.data["subtype"] == "success", first.data

    # ── ① 初始 provider key：掩码 + hint + 响应里搜不到真值 ──
    current = await _settings(http)
    assert current["values"]["providers"][0]["api_key"] is None, current["values"]
    assert current["secrets"]["providers[0].api_key"] == {
        "state": "set",
        "hint": INITIAL_KEY[-4:],
    }, current["secrets"]
    _assert_not_echoed(probe, "/api/settings/get", INITIAL_KEY)

    # ── ② 嵌套密文（gateway.auth.keys[]）也进状态表、也不回显 ──
    document = current["values"]
    document["gateway"]["auth"]["keys"] = [{"key": AUTH_KEY, "role": "admin"}]
    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert set(receipt["changed"]) == {
        "gateway.auth.keys[0].key",
        "gateway.auth.keys[0].role",
    }, receipt["changed"]
    _assert_not_echoed(probe, "/api/settings/set", AUTH_KEY)
    _assert_not_echoed(probe, "/api/settings/set", INITIAL_KEY)

    current = await _settings(http)
    assert current["secrets"]["gateway.auth.keys[0].key"] == {
        "state": "set",
        "hint": AUTH_KEY[-4:],
    }, current["secrets"]
    assert current["values"]["gateway"]["auth"]["keys"][0]["key"] is None, current[
        "values"
    ]
    _assert_not_echoed(probe, "/api/settings/get", AUTH_KEY)

    # ── ③ 字符串覆盖：盘上换新值、响应里搜不到、会话照常 ──
    document = current["values"]
    document["providers"][0]["api_key"] = NEW_KEY
    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["changed"] == ["providers[0].api_key"], receipt["changed"]
    _assert_not_echoed(probe, "/api/settings/set", NEW_KEY)

    written = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    assert written["providers"][0]["api_key"] == NEW_KEY, written["providers"][0]
    assert written["gateway"]["auth"]["keys"][0]["key"] == AUTH_KEY, written["gateway"]

    second = await session.chat("after the key rotation")
    assert second.data["subtype"] == "success", second.data
    assert probe.request(SECRETS_MODEL, 1).context().messages[-1].content == (
        "after the key rotation"
    )

    # ── ④ `null` = 保留：只改别的字段，盘上 key 原样 ──
    current = await _settings(http)
    assert current["values"]["providers"][0]["api_key"] is None, current["values"]
    assert current["secrets"]["providers[0].api_key"] == {
        "state": "set",
        "hint": NEW_KEY[-4:],
    }, current["secrets"]

    document = current["values"]
    document["agents"][0]["keep_recent_tokens"] = 40_000
    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["changed"] == ["agents[0].keep_recent_tokens"], receipt["changed"]

    written = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    assert written["providers"][0]["api_key"] == NEW_KEY, written["providers"][0]
    assert written["agents"][0]["keep_recent_tokens"] == 40_000, written["agents"][0]

    third = await session.chat("after the null round-trip")
    assert third.data["subtype"] == "success", third.data
    assert probe.request(SECRETS_MODEL, 2).context().messages[-1].content == (
        "after the null round-trip"
    )
    session.watch.assert_never("error")

    # 断言强度（如实声明）：假 Provider 不校验凭据（LoggedRequest 无 headers），
    # "key 被真的用上"的直接证据不在本场景——这里钉的是「真值不出网关」与
    # 「null 不改变磁盘上的值」，两者都是 §7.5 红线的可观测面。
