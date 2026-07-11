"""Tests for SessionInitEvent — session 初始化事件。

验证 _push_sync() 在 subscribe 时 emit SessionInitEvent，
以及事件字段正确性。
"""

from __future__ import annotations

from typing import Any

import pytest

from wing.event import SessionInitEvent
from wing.event_bus import event_bus


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    """每个测试前后清理全局 EventBus。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def runtime():
    """创建 WingRuntime 实例。"""
    from wing.runtime import WingRuntime

    return WingRuntime()


class TestSessionInitEvent:
    """SessionInitEvent emit 测试。"""

    @pytest.mark.asyncio
    async def test_session_init_emitted_on_subscribe(self, runtime: Any):
        """subscribe 时 _push_sync() emit SessionInitEvent。"""
        session = runtime.create_session()

        received: list = []
        event_bus.subscribe(lambda e: received.append(e))
        runtime.subscribe("test-client", session.session_id)

        # Find SessionInitEvent
        init_events = [e for e in received if isinstance(e, SessionInitEvent)]
        assert len(init_events) == 1

        init_event = init_events[0]
        assert init_event.type == "session_init"
        assert init_event.session_id == session.session_id
        assert isinstance(init_event.tools, list)
        assert isinstance(init_event.model, str)
        assert init_event.model != ""
        assert init_event.permission_mode in ("default", "bypassPermissions")
        assert isinstance(init_event.cwd, str)

    @pytest.mark.asyncio
    async def test_session_init_tools_list(self, runtime: Any):
        """SessionInitEvent.tools 包含 agent 的工具名。"""
        session = runtime.create_session()

        received: list = []
        event_bus.subscribe(lambda e: received.append(e))
        runtime.subscribe("test-client", session.session_id)

        init_events = [e for e in received if isinstance(e, SessionInitEvent)]
        assert len(init_events) == 1

        tools = init_events[0].tools
        assert isinstance(tools, list)
        assert len(tools) > 0

    @pytest.mark.asyncio
    async def test_session_init_permission_mode_yolo(self, runtime: Any):
        """yolo=true 时 permission_mode 为 'bypassPermissions'。"""
        from wing.gateway.protocol import AgentOverride

        session = runtime.create_session()

        # Set yolo via override
        override = AgentOverride(yolo=True)
        session.apply_agent_override(override)

        received: list = []
        event_bus.subscribe(lambda e: received.append(e))
        runtime.subscribe("test-client", session.session_id)

        init_events = [e for e in received if isinstance(e, SessionInitEvent)]
        assert len(init_events) == 1
        assert init_events[0].permission_mode == "bypassPermissions"

    @pytest.mark.asyncio
    async def test_session_init_permission_mode_default(self, runtime: Any):
        """yolo=false 时 permission_mode 为 'default'。"""
        session = runtime.create_session()

        received: list = []
        event_bus.subscribe(lambda e: received.append(e))
        runtime.subscribe("test-client", session.session_id)

        init_events = [e for e in received if isinstance(e, SessionInitEvent)]
        assert len(init_events) == 1
        assert init_events[0].permission_mode == "default"

    def test_session_init_serialization(self):
        """SessionInitEvent 可序列化为 JSON。"""
        event = SessionInitEvent(
            session_id="test-123",
            tools=["Read", "Write", "Edit", "Bash"],
            model="gpt-4o",
            permission_mode="bypassPermissions",
            cwd="/home/user",
        )
        data = event.model_dump()
        assert data["type"] == "session_init"
        assert data["tools"] == ["Read", "Write", "Edit", "Bash"]
        assert data["model"] == "gpt-4o"
        assert data["permission_mode"] == "bypassPermissions"
        assert data["cwd"] == "/home/user"
