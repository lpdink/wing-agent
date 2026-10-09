"""列表编辑的整机效果：新增 provider 立刻能驱动会话；删空列表报精确问题。

``providers`` / ``agents`` 是**全结构编辑**的对象列表（总设计 D6）。保存事务是
「全文档替换」，列表的增删在下标层面会连带改很多路径（D4 的口径），本场景不看
``changed`` 的细节，只看两个结局：

1. 新增一个 provider（含新 model id）→ ``GET /api/models`` **立刻**出现它，
   且**新模型真的能建会话、跑一轮**（目录是配置的静态投影，但没有运行期证据
   就分不清"目录里有它"和"它真的能驱动模型调用"）；
2. 删空 ``agents`` / 删空 ``providers`` → ``ok=false`` + 精确 path 的 ``empty_list``
   （跨字段检查的两条）；把文档恢复回去 → 又能保存成功（失败不污染后续）。
"""

from __future__ import annotations

import pytest

from wing_probe import DEFAULT_PROBE_MODEL, Probe, Turn

#: 场景私有 model 名：模板模型（env 声明）与新增 provider 的模型。
BASE_MODEL = "probe/settings-list-base"
ADDED_MODEL = "probe/settings-list-added"

#: 新 provider 的名字（runtime 事实维度；与 id 空间无关）。
ADDED_PROVIDER = "probe-added"


def _provider_block(probe: Probe) -> dict:
    """新 provider 的声明（接线到同一个假 Provider——目录/端点归属不是本场景的断言面）。"""
    return {
        "name": ADDED_PROVIDER,
        "protocol": "openai",
        "base_url": probe.env.provider.base_url,
        "api_key": "probe-key",
        "models": [ADDED_MODEL],
    }


@pytest.mark.probe_env(models=[BASE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_added_provider_serves_a_session(probe: Probe) -> None:
    """新增 provider + 新 model id → 目录立刻出现 → 用它跑一轮成功。"""
    probe.register(ADDED_MODEL, Turn.of(text="added provider reply"))
    http = probe.driver_required.http

    current = await http.request("GET", "/api/settings/get")
    document = current["values"]
    document["providers"].append(_provider_block(probe))

    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["restart_required"] == [], receipt
    assert {"providers[1].name", "providers[1].models[0]"} <= set(receipt["changed"]), (
        receipt["changed"]
    )

    # 目录是声明的静态投影：两个 provider 两级都在（顺序 = 声明序）。
    catalog = await http.get_models()
    by_provider = {entry["provider"]: entry["models"] for entry in catalog["providers"]}
    assert list(by_provider) == ["probe", ADDED_PROVIDER], catalog
    assert [model["id"] for model in by_provider[ADDED_PROVIDER]] == [ADDED_MODEL], (
        catalog
    )
    # 既有 provider 的声明序原样（场景声明的模板模型 + 基建补进声明的 probe/default）。
    assert [model["id"] for model in by_provider["probe"]] == [
        BASE_MODEL,
        DEFAULT_PROBE_MODEL,
    ], catalog

    # 新模型真的能驱动一轮调用（唯一硬证据）。
    session = await probe.session(model=ADDED_MODEL)
    result = await session.chat("hello added provider")
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "added provider reply", result.data
    request = probe.request(ADDED_MODEL, 0)
    assert request.body["model"] == ADDED_MODEL, request.body
    session.watch.assert_never("error")


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_empty_lists_are_reported_and_the_document_recovers(probe: Probe) -> None:
    """删空 agents / providers → 精确 empty_list；恢复后又能保存。"""
    http = probe.driver_required.http
    current = await http.request("GET", "/api/settings/get")
    fingerprint = current["fingerprint"]

    # ── 删掉唯一的 agent ──
    document = dict(current["values"])
    document["agents"] = []
    receipt = await http.request(
        "POST", "/api/settings/set", body={"base": fingerprint, "document": document}
    )
    assert receipt["ok"] is False, receipt
    assert [(p["path"], p["kind"]) for p in receipt["problems"]] == [
        ("agents", "empty_list")
    ], receipt["problems"]
    assert receipt["problems"][0]["hint"], receipt["problems"]

    # ── 删掉全部 provider（模型目录的唯一来源） ──
    document = dict(current["values"])
    document["providers"] = []
    receipt = await http.request(
        "POST", "/api/settings/set", body={"base": fingerprint, "document": document}
    )
    assert receipt["ok"] is False, receipt
    assert [(p["path"], p["kind"]) for p in receipt["problems"]] == [
        ("providers", "empty_list")
    ], receipt["problems"]

    # ── 恢复：文档与盘上一致 → 保存成功且 changed 为空（失败不污染后续） ──
    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": fingerprint, "document": current["values"]},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["changed"] == [], receipt
