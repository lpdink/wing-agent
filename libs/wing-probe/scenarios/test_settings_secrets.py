"""密文（api_key）的只写不回显：get 搜不到真值 / hint 是末 4 位 / `null` = 保留。

密文语义（总设计 §7.5）的契约红线：**前端必须原样回传 `null`**——`null` = 保留磁盘
现值，丢键 = 从文件移除（必填字段随即成为 problem）。保存路径上这个分支写错
（例如把 `null` 当成"空串"或"删除键"）不会有任何直观报错：用户下次调用才 401。

覆盖的断言点：

- `get`：`values.providers[0].api_key is None`（掩码），**原始响应文本里搜不到真实 key**，
  `secrets` 表给出 `{"state": "set", "hint": <末 4 位>}`；
- 嵌套密文同样受控：文档里加一条 `gateway.auth.keys[]`（`enabled` 保持 false）后
  `gateway.auth.keys[0].key` 也进 `secrets` 表、真值同样不出现在响应里；
- 字符串覆盖生效：改成新 key → 响应体搜不到新 key、盘上文件是真值，
  且**下一个请求真的带上了新 key**（`Authorization: Bearer <key>`，见下）；
- **`null` 保留原值**：再 `get`（key 是 `null`）→ 只改另一个字段 → 盘上仍是新 key
  （不是 null / 空串）、**下一个请求的 `Authorization` 仍是那把 key**——"改别的字段
  之后旧凭据照旧可用"于是可观测（假 Provider 不校验凭据，但"网关发出去的确实是
  那把钥匙"就是这条要求的可观测等价物，比"盘上还写着"更强）。

**证据面**：`LoggedRequest.headers`（假 Provider 留档的入站请求头，小写键）。
"""

from __future__ import annotations

from typing import Any

import pytest
import yaml

from wing_probe import HttpCall, Probe, Turn

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


def _last_call(probe: Probe, path: str) -> HttpCall:
    """最近一次某路径的留档调用（缺失即失败——"没有泄露"不能建在空留档上）。"""
    call = probe.last_http_call(path=path)
    assert call is not None, f"no recorded call for {path}"
    return call


def _assert_not_echoed(probe: Probe, path: str, secret: str) -> None:
    """真实密钥不得出现在该端点的响应里（原始文本 + 解析后的对象两路搜）。"""
    call = _last_call(probe, path)
    assert secret not in call.text, f"{secret!r} leaked in {path} response text"
    # 密文值也可能被序列化进任何 JSON 字段（如 problems 的 hint）——再搜响应对象。
    assert secret not in str(call.response), (path, call.response)


def _wire_credential(probe: Probe, index: int) -> str | None:
    """第 ``index`` 次 LLM 请求实际发出的 ``Authorization`` 头（小写键留档）。"""
    request = probe.request(SECRETS_MODEL, index)
    return request.headers.get("authorization")


@pytest.mark.probe_env(models=[SECRETS_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_secrets_are_never_echoed_and_null_keeps_the_value(probe: Probe) -> None:
    """get 不回显真值；set 的 `null` 保留磁盘现值、字符串覆盖生效、线格式凭据跟随。"""
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
    # 基线：网关发给上游的凭据就是配置里那把（留档机制本身也要有对照）。
    assert _wire_credential(probe, 0) == f"Bearer {INITIAL_KEY}", probe.request(
        SECRETS_MODEL, 0
    ).headers

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

    # ── ③ 字符串覆盖：盘上换新值、响应里搜不到、**线格式凭据跟着换**、会话照常 ──
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
    # provider 实例按新配置重建：新 key 真的被送上 wire（旧实例会继续发 INITIAL_KEY）。
    assert _wire_credential(probe, 1) == f"Bearer {NEW_KEY}", probe.request(
        SECRETS_MODEL, 1
    ).headers

    # ── ④ `null` = 保留：只改别的字段，盘上 key 原样、线格式凭据也原样 ──
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
    # 任务书点名的子项：改别的字段后再跑一轮，旧凭据照旧可用——可观测形态就是
    # "这一轮实际发出的还是那把 key"（`null` 若被误当成清空/删键，这里就会变成
    # `Bearer None` / 空串 / 保存直接失败）。
    assert _wire_credential(probe, 2) == f"Bearer {NEW_KEY}", probe.request(
        SECRETS_MODEL, 2
    ).headers
    session.watch.assert_never("error")

    # 断言强度（如实声明）：假 Provider 不校验凭据（401 语义不在 probe 层）。
    # 本场景钉的是"网关实际发出的凭据 = 磁盘上的值"（`Authorization` 逐字）与
    # "真值不出网关"两面——前者是"仍能用原 key 认证成功"的可观测等价物：
    # 凭据正确且确实被用于下一次调用，认证成功与否只取决于上游（不在 probe 内）。


# ─────────────────────────────────────────────────────────────────────────────
# A1 回归：删掉列表首项后，剩下的 provider 必须拿到**自己的** key
#
# 按下标回填的旧行为：`providers[0]` 删除后，`providers[1]`（掩码为 null）会从
# `current[0]` 抄来 p1 的密钥——保存成功、回执不报告，用户下次调用才 401 / 打错账号。
# 结构性看不见的原因：既有场景全是单 provider 单 key。这里用**两个 provider、
# 两把不同的 key**，断言「网关实际发出的 Authorization == 那一项自己的 key」。
# ─────────────────────────────────────────────────────────────────────────────

#: 附加 provider（第二个）——同名会撞 provider 全局唯一性，所以用独立名字。
P2_NAME = "probe2"
#: p2 的模型（p1 的模板模型不会与它撞 id）。
P2_MODEL = "probe2/secret-target"
#: p2 自己的 key（与 p1 的 `probe-key` 不同；末 4 位可辨识）。
P2_KEY = "probe2-secret-BBB2"
#: p2 的端点路径（请求留档据此判定"这次调用打到哪个 provider"）。
P2_PATH = f"/{P2_NAME}/v1/chat/completions"


def _wire_credential_for(probe: Probe, model: str, index: int) -> str | None:
    """第 ``index`` 次打到 ``model`` 的 LLM 请求实际发出的 ``Authorization`` 头。"""
    request = probe.request(model, index)
    assert request.path == P2_PATH, (request.path, request.describe())
    return request.headers.get("authorization")


@pytest.mark.probe_env(
    model=SECRETS_MODEL,
    extra_providers=[{"name": P2_NAME, "models": [P2_MODEL], "api_key": P2_KEY}],
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_secret_follows_its_provider_when_the_first_is_deleted(
    probe: Probe,
) -> None:
    """删掉 ``providers[0]`` 后保存：p2 的 key 必须还是 p2 自己的（线格式 + 盘上双证）。"""
    probe.register(P2_MODEL, Turn.of(text="p2-r1"), Turn.of(text="p2-r2"))
    http = probe.driver_required.http

    # 会话用 p2 的模型：请求留档的 path 判定归属（/probe2/v1/chat/completions）。
    session = await probe.session(model=P2_MODEL)

    # ── 控制组：删除前，p2 发出的就是 p2 自己的 key ──
    first = await session.chat("before the delete")
    assert first.data["subtype"] == "success", first.data
    assert _wire_credential_for(probe, P2_MODEL, 0) == f"Bearer {P2_KEY}"

    current = await _settings(http)
    document = current["values"]
    assert [p["name"] for p in document["providers"]] == ["probe", P2_NAME]
    assert document["providers"][0]["api_key"] is None, document["providers"][0]
    assert document["providers"][1]["api_key"] is None, document["providers"][1]

    # ── 面板动作：删掉 providers[0]，回传整份掩码文档 ──
    # 默认 agent 模板引用 p1 的模型——删掉 p1 之前把它改到 p2 的模型上，
    # 让保存后的配置仍然合法（本场景只考察密钥归属）。
    document["agents"][0]["model"] = P2_MODEL
    del document["providers"][0]
    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    # 身份（name）配对成功 ⇒ 没有丢任何东西 ⇒ 不打扰用户。
    assert receipt["warnings"] == [], receipt

    # 盘上：只剩 p2，且 api_key 是 p2 自己的（错配形态：p1 的 probe-key）。
    written = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    assert [p["name"] for p in written["providers"]] == [P2_NAME], written["providers"]
    assert written["providers"][0]["api_key"] == P2_KEY, written["providers"][0]

    # ── 线格式：下一轮对话真的带着 p2 自己的 key 出去 ──
    second = await session.chat("after deleting the first provider")
    assert second.data["subtype"] == "success", second.data
    assert probe.request(P2_MODEL, 1).context().messages[-1].content == (
        "after deleting the first provider"
    )
    assert _wire_credential_for(probe, P2_MODEL, 1) == f"Bearer {P2_KEY}"
    session.watch.assert_never("error")


# ─────────────────────────────────────────────────────────────────────────────
# B1 回归（rev-r1）：(b) 等长下标回落**不许**把已认领槽位的密钥抄给新项
#
# 旧 (b) 只看「长度相等」，不看哪些槽位已被 (a) 身份配对认领：删 p1 + 同一次保存里
# 追加新项 p3（api_key 是 null 哨兵、长度仍相等）时，p3 会静默继承 p2 的密钥——
# ok=true / warnings=[] / changed 不含该路径（与原始 A1 同构的零感知）。
# ─────────────────────────────────────────────────────────────────────────────

#: 追加的新 provider（不被调用：本场景只考密钥归属）。
P3_NAME = "probe3"
#: 新 provider 声明的模型 id（与 p1 / p2 的都不撞）。
P3_MODEL = "probe3/appended"


@pytest.mark.probe_env(
    model=SECRETS_MODEL,
    extra_providers=[{"name": P2_NAME, "models": [P2_MODEL], "api_key": P2_KEY}],
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_appending_a_null_item_is_refused_not_hijacked(probe: Probe) -> None:
    """删 p1 + 同一次保存里加 p3(null)：保存被拦（不写盘），p2 的密钥不受影响。

    三件套断言：回执点名 ``providers[1].api_key``（必填缺失）+ 丢弃警告 + 文件逐字节未变；
    线格式断言：被拒的保存没有污染运行中的网关——下一轮对话仍发 p2 自己的 key。
    """
    probe.register(P2_MODEL, Turn.of(text="p2-r1"), Turn.of(text="p2-r2"))
    http = probe.driver_required.http

    session = await probe.session(model=P2_MODEL)
    first = await session.chat("before the refused save")
    assert first.data["subtype"] == "success", first.data
    assert _wire_credential_for(probe, P2_MODEL, 0) == f"Bearer {P2_KEY}"

    current = await _settings(http)
    document = current["values"]
    assert [p["name"] for p in document["providers"]] == ["probe", P2_NAME]
    before = probe.env.config_path.read_bytes()

    del document["providers"][0]  # 删 p1（它引用的模型同时被替换到 p2 的模型上）
    document["agents"][0]["model"] = P2_MODEL
    document["providers"].append(
        {
            "name": P3_NAME,
            "protocol": "openai",
            "base_url": document["providers"][0]["base_url"],  # 复用 p2 的端点
            "api_key": None,  # 新项的密钥是 null 哨兵——B1 的触发形态
            "models": [P3_MODEL],
        }
    )

    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is False, receipt
    assert [p["path"] for p in receipt["problems"]] == ["providers[1].api_key"], receipt
    assert receipt["warnings"] == [
        "无法确定 providers[1].api_key 属于哪一项（列表结构变化且无法按身份配对），"
        "已移除该密钥，请重新填写"
    ], receipt
    # 全有或全无：文件一个字节都没写（p3 没有拿到 p2 的密钥，也没有拿到别的什么）。
    assert probe.env.config_path.read_bytes() == before

    # 线格式：p2 仍然发自己的 key（被拒的保存没有污染运行中的配置）。
    second = await session.chat("after the refused save")
    assert second.data["subtype"] == "success", second.data
    assert _wire_credential_for(probe, P2_MODEL, 1) == f"Bearer {P2_KEY}"
    session.watch.assert_never("error")
