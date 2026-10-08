"""未命中 id 的错误面 —— 400 + 自解释文案（C7），且状态零变化。

``model_id`` 是唯一引用词，"命中 / 未命中"是二值判断：没有候选集合、没有优先级、
没有回落。外部编排方（ACP / stdio / TUI）拿到的错误必须是**自解释**的，否则一次
配置对齐要来回试错。钉的不变量：

- 未知 id → 400，文案含 ``unknown model id '<值>'; available ids: …``；
- available 列表超过 10 个截断为前 10 个 + ``…``（不刷屏，也仍然可查）；
- 请求值恰是某声明的**调用名**时给出 ``note: … is the call name of model id …``
  提示（列出该名字对应的 id 与 provider）——**只提示，不自动生效**；
- 失败**零副作用**：会话三元组不变、不落任何新记录、不发状态变更事件；
- legacy 字段（``model`` / ``provider``）被静默忽略：纯 legacy body 等价于
  "没有任何更新字段"，与合法字段并存时不干扰（不报错、不生效）。
"""

from __future__ import annotations

import pytest

from wing_probe import DriverHttpError, Probe, Turn

#: id ≠ name：调用名提示要有东西可提示。
FLASH_ID = "flash"
FLASH_NAME = "upstream-flash-v2"
FLASH_SPEC = {"id": FLASH_ID, "name": FLASH_NAME, "display_name": "Flash"}

#: 第二个 id（切换目标 / available 列表的第二项）。
PRO_ID = "pro-alias"
PRO_NAME = "pro-upstream"
PRO_SPEC = {"id": PRO_ID, "name": PRO_NAME}

#: 基建追加的模板 model（id = name）。
TEMPLATE_ID = "probe/default"

#: 截断用例的声明条数（> 10 才会截断）。
WIDE_MODELS = [f"probe/c7-{index}" for index in range(12)]


async def _update(probe: Probe, session_id: str, **fields: object) -> dict:
    """``POST /api/session/update`` 的留档调用（非 2xx 抛 ``DriverHttpError``）。"""
    return await probe.driver_required.http.request(
        "POST", "/api/session/update", body={"session_id": session_id, **fields}
    )


@pytest.mark.probe_env(models=[FLASH_SPEC, PRO_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_unknown_model_id_is_self_explanatory_and_side_effect_free(
    probe: Probe,
) -> None:
    """未知 id 400：available ids + 调用名提示；失败不改任何状态。"""
    probe.register(FLASH_NAME, Turn.of(text="reply"))
    http = probe.driver_required.http
    session = await probe.session(model=FLASH_ID)
    await session.chat("hello")

    changed_before = len(session.watch.events(type="session_state_changed"))
    record_lines_before = list(session.history.record_lines)

    # ① 请求值恰是某声明的调用名：文案要点出它对应哪个 id（一次改对）。
    with pytest.raises(DriverHttpError) as call_name:
        await _update(probe, session.session_id, model_id=FLASH_NAME)
    assert call_name.value.status == 400, call_name.value.call.render()
    detail = call_name.value.call.response["detail"]
    assert f"unknown model id '{FLASH_NAME}'" in detail, detail
    assert f"available ids: {FLASH_ID}, {PRO_ID}, {TEMPLATE_ID}" in detail, detail
    assert (
        f"note: '{FLASH_NAME}' is the call name of model id '{FLASH_ID}' "
        f"(provider 'probe')" in detail
    ), detail
    assert f"send '{FLASH_ID}'" in detail, detail

    # ② 既不命中 id 也不命中调用名：照样报错，但不编造提示。
    with pytest.raises(DriverHttpError) as ghost:
        await _update(probe, session.session_id, model_id="ghost")
    assert ghost.value.status == 400, ghost.value.call.render()
    ghost_detail = ghost.value.call.response["detail"]
    assert "unknown model id 'ghost'" in ghost_detail, ghost_detail
    assert "note:" not in ghost_detail, ghost_detail

    # ③ 状态零变化：三元组照旧、没有新记录、没有状态变更事件。
    info = await http.get_session_info(session.session_id)
    assert info["model_id"] == FLASH_ID, info
    assert info["model"] == FLASH_NAME, info
    assert info["provider_name"] == "probe", info
    assert list(session.history.record_lines) == record_lines_before, "失败不得落盘"
    assert len(session.watch.events(type="session_state_changed")) == changed_before


@pytest.mark.probe_env(models=[FLASH_SPEC, PRO_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_update_fields_are_silently_ignored(probe: Probe) -> None:
    """legacy ``{model, provider}`` 静默忽略：纯 legacy = 无更新字段；不干扰合法字段。"""
    http = probe.driver_required.http
    session = await probe.session(model=FLASH_ID)

    # 纯 legacy body：字段被忽略后没有任何更新字段 → 400（不是"未知模型"）。
    with pytest.raises(DriverHttpError) as legacy:
        await _update(probe, session.session_id, model="ghost", provider="ghost")
    assert legacy.value.status == 400, legacy.value.call.render()
    assert (
        legacy.value.call.response["detail"] == "at least one update field is required"
    ), legacy.value.call.response

    # 与合法字段并存：合法字段生效，legacy 字段既不报错也不生效。
    payload = await _update(
        probe, session.session_id, model_id=PRO_ID, model="ghost", provider="ghost"
    )
    assert payload.get("ok") is True, payload
    info = await http.get_session_info(session.session_id)
    assert info["model_id"] == PRO_ID, info
    assert info["model"] == PRO_NAME, info
    assert info["provider_name"] == "probe", info


@pytest.mark.probe_env(models=WIDE_MODELS)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_available_ids_are_truncated_past_ten(probe: Probe) -> None:
    """声明超过 10 个 id：available 列表截断为前 10 个 + ``…``。"""
    session = await probe.session(model="probe/c7-0")
    declared = [*WIDE_MODELS, TEMPLATE_ID]
    assert len(declared) == 13

    with pytest.raises(DriverHttpError) as failure:
        await _update(probe, session.session_id, model_id="nope")
    assert failure.value.status == 400, failure.value.call.render()
    detail = failure.value.call.response["detail"]
    expected = ", ".join(declared[:10])
    assert f"available ids: {expected}, …" in detail, detail
    assert declared[10] not in detail, detail
