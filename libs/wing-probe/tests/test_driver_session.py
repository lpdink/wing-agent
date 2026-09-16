"""driver 会话挂载语义单测（不起网关、不发请求）。

覆盖 ``Driver.attach`` 的三条契约（场景 fixture 与失败报告依赖它们）：

1. 挂载即把 session 注册进事件路由表，**并且**把 ``watch.dump_path`` 指向
   ``<env.root>/artifacts``——``expect`` 超时报告末行因此能引用现场转储目录
   （design D8；转储由 ``probe.dump()`` / fixture teardown 落到同一目录）；
2. 重复挂载返回同一个句柄（不重建时间线、不丢事件）；
3. 路由：``_on_event`` 把带 session_id 的事件投进该 session 的时间线，
   未知 session 的事件进 driver 级时间线（诊断用）。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing_probe.driver import Driver, Delivery


class FakeEnv:
    """driver 依赖的 ``ProbeEnv`` 最小面（``EnvLike`` 协议）。"""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.gateway_url = "http://127.0.0.1:1"
        self.started_at = 0.0

    @property
    def artifacts_path(self) -> Path:
        return self.root / "artifacts"

    def session_dir(self, session_id: str) -> Path:
        return self.root / "sessions" / session_id


@pytest.mark.asyncio
async def test_attach_registers_session_and_dump_path(tmp_path: Path) -> None:
    """挂载注册会话、指向现场转储目录（expect 失败报告末行）。"""
    env = FakeEnv(tmp_path)
    driver = Driver(env)

    session = await driver.attach("sid-1", subscribe=False)

    assert driver.get("sid-1") is session
    assert [item.session_id for item in driver.sessions] == ["sid-1"]
    assert session.watch.dump_path == str(env.artifacts_path)
    assert session.session_dir == env.session_dir("sid-1")
    assert session.timeline.started_at == env.started_at
    await driver.close()


@pytest.mark.asyncio
async def test_attach_is_idempotent(tmp_path: Path) -> None:
    """重复挂载返回同一句柄（时间线与已收事件不丢）。"""
    driver = Driver(FakeEnv(tmp_path))
    first = await driver.attach("sid-1", subscribe=False)
    first.timeline.append("turn_result", {"session_id": "sid-1"})

    second = await driver.attach("sid-1", subscribe=False)

    assert second is first
    assert [event.type for event in second.timeline.all()] == ["turn_result"]
    await driver.close()


@pytest.mark.asyncio
async def test_events_route_to_session_and_fall_back_to_driver(tmp_path: Path) -> None:
    """带 session_id 的事件进该会话时间线；未知会话进 driver 级时间线。"""
    driver = Driver(FakeEnv(tmp_path))
    session = await driver.attach("sid-1", subscribe=False)

    driver._on_event(
        "sid-1",
        "text",
        {"session_id": "sid-1", "content": "hi"},
        Delivery(text='{"type":"text"}', at=1.0),
    )
    driver._on_event(
        "unknown",
        "notice",
        {"session_id": "unknown"},
        Delivery(text='{"type":"notice"}', at=1.5),
    )
    driver._on_event(
        None,
        "connected",
        {},
        Delivery(text='{"type":"connected"}', at=2.0),
    )

    assert [(event.type, event.data) for event in session.timeline.all()] == [
        ("text", {"session_id": "sid-1", "content": "hi"})
    ]
    assert [event.type for event in driver.timeline.all()] == ["notice", "connected"]
    await driver.close()
