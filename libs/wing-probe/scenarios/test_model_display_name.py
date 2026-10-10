"""模型展示名（``display_name``）随会话状态下发的链路。

展示名是**配置声明**（providers[].models[].display_name）——前端（TUI 状态栏 /
切模型吐司）只消费网关下发的 ``model_display_name``，不拿实际调用名去
``/api/models`` 里自查。四条出口必须同源同刻：

- ``/api/session/info``：``model_display_name`` == 声明值；无声明模型为 null；
- ``/api/session/get`` 的 agent 快照（与 sync_session 同一个 to_agent_info 出口）；
- ``sync_session`` 的 agent 快照（订阅重放路径）：``model_id`` / ``model_name`` /
  ``provider_name`` / ``model_display_name`` 四件套一起下发；
- ``session_state_changed``（换模型直播路径）：与 ``model`` / ``model_id`` 同刻下发
  展示名，无声明模型该字段缺失（wire 剥 null）。

展示名不是身份：``/api/session/info`` 的 ``model`` 始终保持实际调用名——
换一个模型（哪怕展示名相同）也必须改变 ``model``，本场景用"同刻换模型"验证
两者分列两个字段。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间不共享）。
LABELED_MODEL = "probe/display-labeled"
PLAIN_MODEL = "probe/display-plain"

#: provider 静态声明：一个带展示名，一个只有调用名（字符串形态）。
PROBE_PROVIDER = "probe"
LABELED_SPEC = {"name": LABELED_MODEL, "display_name": "Labeled Flash"}
PLAIN_SPEC = PLAIN_MODEL

DISPLAY_NAME = "Labeled Flash"


async def _switch_model(probe: Probe, session_id: str, model_id: str) -> dict:
    """经 HTTP 换模型（``{model_id}`` 是唯一的模型切换入口）。"""
    payload = await probe.driver_required.http.request(
        "POST",
        "/api/session/update",
        body={"session_id": session_id, "model_id": model_id},
    )
    assert payload.get("ok") is True, payload
    return payload


@pytest.mark.probe_env(models=[LABELED_SPEC, PLAIN_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_display_name_travels_with_session_state(probe: Probe) -> None:
    """展示名三条出口（sync / info / 状态变更事件）+ 无声明回落。"""
    probe.register(LABELED_MODEL, Turn.of(text="labeled reply"))
    probe.register(PLAIN_MODEL, Turn.of(text="plain reply"))
    http = probe.driver_required.http

    session = await probe.session(model=LABELED_MODEL)

    # ① 订阅重放：sync_session 的 agent 快照与身份同刻携带展示名。
    sync = await session.watch.expect("sync_session", within=5.0)
    agent = sync.data["agent"]
    assert agent["model_id"] == LABELED_MODEL, agent
    assert agent["model_name"] == LABELED_MODEL, agent
    assert agent["provider_name"] == PROBE_PROVIDER, agent
    assert agent["model_display_name"] == DISPLAY_NAME, agent

    # ② /api/session/info：展示名与身份分列两个字段（id = 引用词，model = 调用名）。
    info = await http.get_session_info(session.session_id)
    assert info["model"] == LABELED_MODEL, info
    assert info["model_id"] == LABELED_MODEL, info
    assert info["model_display_name"] == DISPLAY_NAME, info

    # ②' 会话详情（get）的 agent 快照是同一个 to_agent_info 出口，也一样携带。
    detail = await http.get_session(session.session_id)
    assert detail["agent"]["model_name"] == LABELED_MODEL, detail["agent"]
    assert detail["agent"]["model_display_name"] == DISPLAY_NAME, detail["agent"]

    # ③ 换到无声明展示名的模型：事件与 info 一起回落（字段缺失 / null），
    #    身份字段照常更新——回落由前端做，网关不发空串。
    await _switch_model(probe, session.session_id, PLAIN_MODEL)
    changed = await session.watch.expect("session_state_changed", within=5.0)
    assert changed.data["model"] == PLAIN_MODEL, changed.data
    assert changed.data["model_id"] == PLAIN_MODEL, changed.data
    assert changed.data.get("model_display_name") is None, changed.data

    info = await http.get_session_info(session.session_id)
    assert info["model"] == PLAIN_MODEL, info
    assert info["model_id"] == PLAIN_MODEL, info
    assert info["model_display_name"] is None, info

    # ③' 展示名不是身份：消息仍按**实际调用名**路由到假 Provider（换模型后
    #     的请求打到 PLAIN_MODEL 的剧本，而不是任何展示字面量）。
    result = await session.chat("ping plain")
    assert result.data["subtype"] == "success", result.data
    assert probe.request(PLAIN_MODEL, 0).model == PLAIN_MODEL

    # ④ 换回声明了展示名的模型：同一 provider 内切换，展示名随事件恢复。
    await _switch_model(probe, session.session_id, LABELED_MODEL)
    changed = await session.watch.expect("session_state_changed", within=5.0)
    assert changed.data["model"] == LABELED_MODEL, changed.data
    assert changed.data["model_id"] == LABELED_MODEL, changed.data
    assert changed.data["model_display_name"] == DISPLAY_NAME, changed.data
