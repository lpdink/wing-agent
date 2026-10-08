"""模型目录（``GET /api/models``）的 wire 形状 —— ``id`` 是唯一引用词。

钉的不变量（C2 / C3）：

- 目录是**配置声明的静态投影**（远端 ``/models`` 发现已退役）：无网络、无 IO，
  每次读到的都是同一份声明；
- 每个条目是**对象**，键恰好 ``{id, name, display_name, description,
  capabilities}``——不再是 ``list[str]``，也没有 ``model_details`` 平行数组
  （"两数组逐项对齐"的脆弱契约被消灭）；
- ``id`` 缺省 = ``name``（存量字符串形态零改动）；显式 ``id:`` 时两者**分列**
  （``id`` 是引用词，``name`` 是发给上游的调用名）；
- ``display_name`` / ``description`` / ``capabilities`` 原样投影：未声明 = null /
  全 false（不做名字启发式）；
- 顺序 = 配置声明序（目录只有一个来源，不排序、不回落）；
- **目录是合同不是展示面**：每个列出的 id 都能当 ``update {model_id}`` 用，
  切换后上游收到的是该条目的 ``name``。
"""

from __future__ import annotations

import json

import pytest

from wing_probe import Probe, Turn

#: 存量形态：字符串声明（id = name）。
LEGACY_ID = "probe/catalog-legacy"
#: 对象形态：只给展示元信息（id = name）。
NAMED_ID = "probe/catalog-named"
#: 对象形态：显式 id ≠ name（引用词与调用名分列）。
ALIAS_ID = "catalog-alias"
ALIAS_NAME = "probe/catalog-upstream"

NAMED_SPEC = {
    "name": NAMED_ID,
    "display_name": "Named Model",
    "description": "declares a display name",
    "capabilities": {"vision": True},
}
ALIAS_SPEC = {
    "id": ALIAS_ID,
    "name": ALIAS_NAME,
    "display_name": "Catalog Alias",
}

#: 模板 model（``ProbeEnv`` 的默认值）由基建追加进声明——目录里必须也有它。
TEMPLATE_ID = "probe/default"


def _entry(model_id: str, name: str, **overrides: object) -> dict:
    """目录条目（未声明的字段就是 null / 假）。"""
    entry: dict = {
        "id": model_id,
        "name": name,
        "display_name": None,
        "description": None,
        "capabilities": {"vision": False},
    }
    entry.update(overrides)
    return entry


@pytest.mark.probe_env(models=[LEGACY_ID, NAMED_SPEC, ALIAS_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_catalog_shape_and_usable_ids(probe: Probe) -> None:
    """目录逐条目对齐（对象数组 / id 缺省与显式分列 / 声明序）+ id 可直接使用。"""
    probe.register(ALIAS_NAME, Turn.of(text="alias reply"))
    http = probe.driver_required.http

    catalog = await http.get_models()

    # ① 逐条目对齐：对象数组、键集恰好五项、顺序 = 声明序（模板 model 追加在末尾）。
    assert catalog == {
        "providers": [
            {
                "provider": "probe",
                "models": [
                    _entry(LEGACY_ID, LEGACY_ID),
                    _entry(
                        NAMED_ID,
                        NAMED_ID,
                        display_name="Named Model",
                        description="declares a display name",
                        capabilities={"vision": True},
                    ),
                    _entry(
                        ALIAS_ID,
                        ALIAS_NAME,
                        display_name="Catalog Alias",
                    ),
                    _entry(TEMPLATE_ID, TEMPLATE_ID),
                ],
            }
        ]
    }, catalog

    # ② 没有 model_details 平行数组（任何层级都不该出现）。
    assert "model_details" not in json.dumps(catalog), catalog

    # ③ id 全局唯一（"解析 = 单键查表"的前提）。
    ids = [item["id"] for item in catalog["providers"][0]["models"]]
    assert len(ids) == len(set(ids)), ids

    # ④ 每个 id 都是可用的引用词：切换成功，且生效的调用名 = 目录里的 name。
    session = await probe.session(model=LEGACY_ID)
    for entry in catalog["providers"][0]["models"]:
        await http.update_session(session.session_id, model_id=entry["id"])
        info = await http.get_session_info(session.session_id)
        assert info["model_id"] == entry["id"], info
        assert info["model"] == entry["name"], info

    # ⑤ 目录是合同：切到显式 id 的条目后，上游收到的是它的调用名（不是 id）。
    await http.update_session(session.session_id, model_id=ALIAS_ID)
    result = await session.chat("hello")
    assert result.data["subtype"] == "success", result.data
    logged = probe.request(ALIAS_NAME, 0)
    assert logged.model == ALIAS_NAME, logged.describe()
    assert logged.body["model"] == ALIAS_NAME, logged.body
    assert logged.path == "/v1/chat/completions", logged.path
