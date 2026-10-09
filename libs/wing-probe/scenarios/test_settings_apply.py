"""Setting API 的**生效**证据：改配置 → 下一个 LLM 请求体真的变了。

``/api/system/reload`` 的"改盘上 config.yaml 后 reload 生效"已有场景
（``test_system_reload.py``）；这里钉的是**保存事务**这条路：``GET /api/settings/get``
→ 改两处（透传 ``extra_body`` 与 provider 级 ``reasoning_effort``）→ ``POST /api/settings/set``
→ 假 Provider 留档的**下一个请求体**必须带上它们。只看"文件写了"是不够的：
``extra_body`` 是 provider 实例构造期读进 ``self._extra_body`` 的，重载没重建实例
就会静默失效——只有请求体能抓住。

覆盖的断言点：

- 保存回执：``ok=true`` / ``changed`` 恰为两条路径 / ``restart_required`` 为空（两者都是
  hot 域）/ ``reload.results`` 名字序 == **六项**（R3：``log level`` 追加在末尾）且逐项 ok；
- **单键 map 的 `extra_body` 落盘后文件仍可解析**（AD7 的整机层证据：``emit.py`` 曾把
  "恰好单行的容器载荷"内联成 ``key: a: 1`` 这种非法 YAML，保存一次就把配置写坏）；
- 下一个请求体逐字段对账（marker 出现、``reasoning_effort == "high"``），且该轮照常收尾。
"""

from __future__ import annotations

from typing import Any

import pytest
import yaml

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
APPLY_MODEL = "probe/settings-apply"

#: 透传键（值刻意是单键 map——AD7 的最小复现形态）。
MARKER_KEY = "probe_settings_extra_body"
MARKER_VALUE = "on"

#: 热重载六项（R3：前五项是既有对外契约，`log level` 追加在末尾）。
RELOAD_ITEMS = [
    "config.yaml",
    "hooks",
    "prompt commands",
    "provider",
    "skills & rules",
    "log level",
]


async def _settings(http: Any) -> dict:
    """``GET /api/settings/get``（稀疏文档 + 指纹 + 密文状态 + problems）。"""
    return await http.request("GET", "/api/settings/get")


async def _save(http: Any, base: str, document: dict) -> dict:
    """``POST /api/settings/set``（全文档替换 + 乐观并发指纹）。"""
    return await http.request(
        "POST", "/api/settings/set", body={"base": base, "document": document}
    )


@pytest.mark.probe_env(models=[APPLY_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_extra_body_and_reasoning_effort_reach_the_next_request(
    probe: Probe,
) -> None:
    """保存 extra_body / reasoning_effort → 假 Provider 的下一个请求体真的带上它们。"""
    probe.register(APPLY_MODEL, Turn.of(text="before"), Turn.of(text="after"))
    session = await probe.session(model=APPLY_MODEL)
    http = probe.driver_required.http

    # ① 基线：这一轮请求体里没有两个观测点。
    first = await session.chat("before the save")
    assert first.data["subtype"] == "success", first.data
    body_before = probe.request(APPLY_MODEL, 0).body
    assert MARKER_KEY not in body_before, body_before
    assert "reasoning_effort" not in body_before, body_before

    # ② get → 改两处 → set（整份稀疏文档回传；密文 null 的保留语义见 secrets 场景）。
    #    第二个观测点用 provider 级 `reasoning_effort` 而不是 §18.3 表里写的 `max_tokens`：
    #    `max_tokens` 只进 Anthropic 协议的 body（anthropic/provider.py），openai 协议的
    #    假 Provider 上它**根本不出现在请求体里**，观测不到（review N1）。
    current = await _settings(http)
    provider_doc = current["values"]["providers"][0]
    provider_doc["extra_body"] = {MARKER_KEY: MARKER_VALUE}
    provider_doc["reasoning_effort"] = "high"

    receipt = await _save(http, current["fingerprint"], current["values"])

    # ③ 回执：ok / 六项 reload 名字序 / changed / restart_required。
    assert receipt["ok"] is True, receipt
    assert receipt["problems"] == [], receipt
    reload_result = receipt["reload"]
    assert reload_result["ok"] is True, reload_result
    assert [item["name"] for item in reload_result["results"]] == RELOAD_ITEMS, (
        reload_result["results"]
    )
    assert [item["ok"] for item in reload_result["results"]] == [True] * 6, (
        reload_result["results"]
    )
    assert set(receipt["changed"]) == {
        "providers[0].extra_body",
        "providers[0].reasoning_effort",
    }, receipt["changed"]
    assert receipt["restart_required"] == [], receipt
    assert receipt["setup_mode_exited"] is False, receipt

    # ④ AD7：单键 map 落盘后文件仍可解析（修复前这里是非法 YAML）。
    raw = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    assert raw["providers"][0]["extra_body"] == {MARKER_KEY: MARKER_VALUE}, raw[
        "providers"
    ][0]
    assert raw["providers"][0]["reasoning_effort"] == "high", raw["providers"][0]

    # ⑤ 硬证据：下一个请求体真的变了（provider 实例按新配置重建过）。
    second = await session.chat("after the save")
    assert second.data["subtype"] == "success", second.data
    body_after = probe.request(APPLY_MODEL, 1).body
    assert body_after[MARKER_KEY] == MARKER_VALUE, body_after
    assert body_after["reasoning_effort"] == "high", body_after
    # 前缀身份不受影响：system 段与 tools 声明逐字节一致（改的是 provider 配置，
    # 不是会话历史；各请求的 cache_control 标记落在自己的最后一条消息上，比对跳过）。
    assert body_after["messages"][0] == body_before["messages"][0], (
        body_before["messages"][0],
        body_after["messages"][0],
    )
    assert body_after["tools"] == body_before["tools"], (
        body_before["tools"],
        body_after["tools"],
    )
    session.watch.assert_never("error")
