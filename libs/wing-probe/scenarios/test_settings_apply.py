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
- **单键 map 的 `extra_body` 落盘后文件仍可解析**（整机层证据：``emit.py`` 曾把
  "恰好单行的容器载荷"内联成 ``key: a: 1`` 这种非法 YAML，保存一次就把配置写坏）；
- 下一个请求体逐字段对账（marker 出现、``reasoning_effort == "high"``），且该轮照常收尾。
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest
import yaml

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
APPLY_MODEL = "probe/settings-apply"

#: 透传键（值刻意是单键 map——最小复现形态）。
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
    probe.register(
        APPLY_MODEL, Turn.of(text="before"), Turn.of(text="after"), Turn.of(text="new")
    )
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
    #    假 Provider 上它**根本不出现在请求体里**，观测不到。
    current = await _settings(http)
    provider_doc = current["values"]["providers"][0]
    provider_doc["extra_body"] = {MARKER_KEY: MARKER_VALUE}
    provider_doc["reasoning_effort"] = "high"

    current["values"]["agents"][0]["system_prompt"] = "New chat defaults marker"
    current["values"]["gateway"]["cors"] = {"origins": ["https://ui.probe.example"]}

    receipt = await _save(http, current["fingerprint"], current["values"])

    # ③ 回执：ok / 六项 reload 名字序 / changed / restart_required。
    assert receipt["ok"] is True, receipt
    assert receipt["problems"] == [], receipt
    reload_result = receipt["reload"]
    assert reload_result["ok"] is True, reload_result
    assert reload_result["new_session_defaults_changed"] is True, reload_result
    assert reload_result["pending_restart_paths"] == ["gateway.cors.origins"], (
        reload_result
    )
    assert [item["name"] for item in reload_result["results"]] == RELOAD_ITEMS, (
        reload_result["results"]
    )
    assert [item["ok"] for item in reload_result["results"]] == [True] * 6, (
        reload_result["results"]
    )
    assert set(receipt["changed"]) == {
        "providers[0].extra_body",
        "providers[0].reasoning_effort",
        "agents[0].system_prompt",
        "gateway.cors.origins[0]",
    }, receipt["changed"]
    assert receipt["restart_required"] == ["gateway.cors.origins[0]"], receipt
    assert receipt["setup_mode_exited"] is False, receipt

    # ④ 单键 map 落盘后文件仍可解析（修复前这里是非法 YAML）。
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

    new_session = await probe.session(model=APPLY_MODEL)
    assert (await new_session.chat("new chat")).data["subtype"] == "success"
    new_body = probe.request(APPLY_MODEL, 2).body
    assert "New chat defaults marker" in str(new_body["messages"][0]), new_body
    assert new_body[MARKER_KEY] == MARKER_VALUE, new_body


@pytest.mark.probe_env(models=[APPLY_MODEL])
@pytest.mark.asyncio
async def test_saved_config_with_partial_reload_failure_can_be_retried(
    probe: Probe, tmp_path: Path
) -> None:
    probe.register(
        APPLY_MODEL,
        Turn.of(text="before"),
        Turn.of(text="while broken"),
        Turn.of(text="recovered"),
    )
    session = await probe.session(model=APPLY_MODEL)
    http = probe.driver_required.http
    assert (await session.chat("warm the provider pool")).data["subtype"] == "success"

    marker = "fake-secret-never-in-receipts"
    hook = tmp_path / "broken.py"
    hook.write_text(f"raise RuntimeError({marker!r})", encoding="utf-8")
    command = tmp_path / "broken.md"
    command.write_text(f"---\ndescription: [{marker}\n---\n", encoding="utf-8")
    good_command = tmp_path / "working.md"
    good_command.write_text(
        "---\nname: remaining-command\n---\nWorks.\n", encoding="utf-8"
    )
    current = await _settings(http)
    document = current["values"]
    document["hooks"] = [str(hook)]
    document["commands"] = {"paths": [str(tmp_path / "*.md")]}
    document["providers"][0].update(
        api_key=f"{marker}é", extra_body={MARKER_KEY: MARKER_VALUE}
    )
    receipt = await _save(http, current["fingerprint"], document)
    assert receipt["ok"] and not receipt["reload"]["ok"], receipt
    assert [item["name"] for item in receipt["reload"]["results"]] == RELOAD_ITEMS
    items = {item["name"]: item for item in receipt["reload"]["results"]}
    assert {name for name, item in items.items() if not item["ok"]} == {
        "hooks",
        "prompt commands",
        "provider",
    }
    assert "1 hook file" in items["hooks"]["detail"]
    assert "1 prompt command file" in items["prompt commands"]["detail"]
    assert marker not in str(receipt)
    assert str(hook) not in str(receipt) and str(command) not in str(receipt)
    diagnostics = probe.env.log_path.read_text(encoding="utf-8")
    assert f"{str(hook)!r}:1: RuntimeError" in diagnostics
    assert f"{str(command)!r}: invalid YAML front matter at line" in diagnostics
    assert "provider: UnicodeEncodeError (source " in diagnostics
    assert marker not in diagnostics
    saved = await _settings(http)
    assert saved["fingerprint"] == receipt["fingerprint"]
    assert saved["values"]["providers"][0]["api_key"] is None
    assert "remaining-command" in str(await http.request("GET", "/api/commands"))

    assert (await session.chat("cached provider still works")).data[
        "subtype"
    ] == "success"
    assert MARKER_KEY not in probe.request(APPLY_MODEL, 1).body

    document["providers"][0]["api_key"] = "probe-repaired-key"
    document.pop("hooks")
    document["commands"]["paths"] = [str(good_command)]
    repaired = await _save(http, saved["fingerprint"], document)
    assert repaired["reload"]["ok"]
    assert (await session.chat("provider reloaded")).data["subtype"] == "success"
    assert probe.request(APPLY_MODEL, 2).body[MARKER_KEY] == MARKER_VALUE
