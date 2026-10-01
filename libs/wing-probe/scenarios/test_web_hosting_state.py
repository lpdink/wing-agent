"""web 托管 probe 场景（step 03）："恢复 UI 状态"所需信息已在既有协议面上。

step 03 的协议缺口检查（design D9）结论是**不新增字段**：web 壳恢复控制面状态所需的
素材全部可以经现有事件 / 字段拿到，与 VSCode 扩展 / TUI 同路径。本场景把这句结论变成
可执行证据——它不测新代码，测的是"web 对接时依赖的这几个字段确实在"：

- 订阅重放（`sync_session.agent`）：model / provider / tools —— UI 首帧就要；
- `GET /api/session/info`：thinking / reasoning_effort / yolo —— `sync_session`
  **不带**这三个开关（见 `extensions/vscode/src/host/session/manager.ts` 的
  `refreshRuntimeState` 注释），前端订阅后补拉一次；
- `GET /api/session/list`：name / status / last_interaction / workspace ——
  会话列表（含后台会话的 live 状态）与"接续上次"的判断依据。

这条场景红了只有两种可能：要么字段真的没了（web 会缺一块），要么字段改名（08 步
对接的字段名漂移）——两种都必须在合并前解决。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Turn

#: 会话标题：列表项的 name 取它（不发言的会话不会进列表，标题是拿到列表项的捷径）。
SESSION_TITLE = "web state restore"

#: 显式设置的控制面开关（探"写进去能读回来"的往返）。
REASONING_EFFORT = "high"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_subscribe_replay_carries_model_identity(probe: Probe) -> None:
    """订阅重放：`sync_session.agent` 携带 model / provider / tools。"""
    session = await probe.session()
    http = probe.driver_required.http

    replayed = session.watch.events("sync_session")
    assert replayed, "订阅没有收到 sync_session 重放"
    agent = replayed[0].data["agent"]
    assert agent["model_name"] == probe.env.model, agent
    assert agent["provider_name"] == "probe", agent
    assert agent["workspace"] == str(session.workspace), agent
    assert isinstance(agent["tools"], list) and agent["tools"], agent

    # 与 REST 查询面同源（同一份 agent 快照的两个出口）。
    info = await http.get_session_info(session.session_id)
    assert info["model"] == agent["model_name"], (info, agent)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_runtime_knobs_come_from_session_info(probe: Probe) -> None:
    """控制面开关经 `/api/session/update` 写入后，`/api/session/info` 读得回来。"""
    session = await probe.session()
    http = probe.driver_required.http
    session_id = session.session_id

    updated = await http.request(
        "POST",
        "/api/session/update",
        body={
            "session_id": session_id,
            "title": SESSION_TITLE,
            "thinking": True,
            "reasoning_effort": REASONING_EFFORT,
            "yolo": True,
        },
    )
    assert updated.get("ok") is True, updated

    info = await http.get_session_info(session_id)
    assert info["thinking"] is True, info
    assert info["reasoning_effort"] == REASONING_EFFORT, info
    assert info["yolo"] is True, info
    assert info["model"] == probe.env.model, info
    assert info["workdir"] == str(session.workspace), info
    assert info["session_name"] == SESSION_TITLE, info
    # 前端展示名的回落口径：未声明展示名 = None，前端用 model 顶上。
    assert info.get("model_display_name") is None, info


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_session_list_carries_list_and_switch_fields(probe: Probe) -> None:
    """列表项字段足够渲染"会话列表 + 后台状态"：name / status / 时间 / workspace。"""
    probe.register(probe.env.model, Turn.of(text="noted"))
    session = await probe.session(model=probe.env.model)
    result = await session.chat("hello")
    assert result.data["subtype"] == "success", result.data
    http = probe.driver_required.http
    session_id = session.session_id
    await http.request(
        "POST",
        "/api/session/update",
        body={"session_id": session_id, "title": SESSION_TITLE},
    )

    listing = await http.request("GET", "/api/session/list")
    entries = [entry for entry in listing["sessions"] if entry["id"] == session_id]
    assert len(entries) == 1, listing["sessions"]
    entry = entries[0]
    assert entry["name"] == SESSION_TITLE, entry
    # 已加载 → live 状态（inactive 只属于不在内存的会话）。
    assert entry["status"] in {"idle", "working", "waiting"}, entry
    assert entry["last_interaction"], entry
    assert entry["workspace"] == str(session.workspace), entry

    # 逐出后同一字段反映 inactive（后台会话的状态可观测——web 列表据此显示"已挂起"）。
    driver = probe.driver_required
    await driver.http.unsubscribe(session_id, driver.client_id)
    released = await driver.http.request(
        "POST", "/api/session/release", body={"session_id": session_id}
    )
    assert released["ok"] is True, released
    after = await driver.http.request("GET", "/api/session/list")
    entry_after = next(item for item in after["sessions"] if item["id"] == session_id)
    assert entry_after["status"] == "inactive", entry_after
    assert entry_after["name"] == SESSION_TITLE, entry_after
