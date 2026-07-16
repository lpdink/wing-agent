"""EventBus 单元测试——验证路由表、emit、scope 转换等行为逻辑."""

import pytest

from wing.event import (
    EventTarget,
    DeliveredEvent,
    TextEvent,
)
from wing.event_bus import EventBus
from wing.request_context import (
    reset_request_context,
    set_request_context,
)


@pytest.fixture
def bus():
    """创建干净的 EventBus 实例（不使用全局单例，避免测试间干扰）."""
    return EventBus()


class TestRoutingTable:
    """路由表操作：route_attach / route_detach / route_detach_client."""

    def test_route_attach(self, bus: EventBus):
        """route_attach 后，路由表记录 client → session 关联."""
        bus.route_attach("client-a", "session-1")
        assert bus.routing_table == {"client-a": {"session-1"}}

    def test_route_attach_multiple_sessions(self, bus: EventBus):
        """一个 client 可以订阅多个 session."""
        bus.route_attach("client-a", "session-1")
        bus.route_attach("client-a", "session-2")
        assert bus.routing_table == {"client-a": {"session-1", "session-2"}}

    def test_route_attach_multiple_clients(self, bus: EventBus):
        """多个 client 可以订阅同一个 session."""
        bus.route_attach("client-a", "session-1")
        bus.route_attach("client-b", "session-1")
        assert bus.routing_table == {
            "client-a": {"session-1"},
            "client-b": {"session-1"},
        }

    def test_route_detach(self, bus: EventBus):
        """route_detach 移除指定 client → session 关联."""
        bus.route_attach("client-a", "session-1")
        bus.route_attach("client-a", "session-2")
        bus.route_detach("client-a", "session-1")
        assert bus.routing_table == {"client-a": {"session-2"}}

    def test_route_detach_last_session_removes_client(self, bus: EventBus):
        """client 的最后一个 session detach 后，client 从路由表删除."""
        bus.route_attach("client-a", "session-1")
        bus.route_detach("client-a", "session-1")
        assert bus.routing_table == {}

    def test_route_detach_nonexistent(self, bus: EventBus):
        """detach 不存在的关联不报错."""
        bus.route_detach("client-x", "session-y")
        assert bus.routing_table == {}

    def test_route_detach_client(self, bus: EventBus):
        """route_detach_client 移除 client 所有关联."""
        bus.route_attach("client-a", "session-1")
        bus.route_attach("client-a", "session-2")
        bus.route_detach_client("client-a")
        assert bus.routing_table == {}

    def test_route_detach_client_nonexistent(self, bus: EventBus):
        """detach 不存在的 client 不报错."""
        bus.route_detach_client("client-x")
        assert bus.routing_table == {}


class TestEmitScopeSession:
    """scope="session"：EventBus 查路由表，转换为 scope="client" + client_ids."""

    def test_session_scope_with_matching_route(self, bus: EventBus):
        """session 事件发给订阅了该 session 的 client."""
        bus.route_attach("client-a", "session-1")
        bus.route_attach("client-b", "session-1")
        bus.route_attach("client-c", "session-2")

        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.emit(
            TextEvent(
                session_id="session-1",
                content="hello",
                target=EventTarget(scope="session"),
            )
        )

        assert len(received) == 1
        event = received[0]
        assert event.target.scope == "client"
        assert set(event.target.client_ids) == {"client-a", "client-b"}

    def test_session_scope_no_matching_route(self, bus: EventBus):
        """session 事件没有订阅者 → client_ids 为空."""
        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.emit(
            TextEvent(
                session_id="session-unknown",
                content="hello",
                target=EventTarget(scope="session"),
            )
        )

        assert len(received) == 1
        event = received[0]
        assert event.target.scope == "client"
        assert event.target.client_ids == []

    def test_session_scope_no_session_id(self, bus: EventBus):
        """session 事件没有 session_id → client_ids 为空."""
        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.emit(
            TextEvent(
                content="hello",
                target=EventTarget(scope="session"),
            )
        )

        assert len(received) == 1
        assert received[0].target.client_ids == []


class TestEmitScopeGlobal:
    """scope="global"：事件发给所有 subscriber，target 不变."""

    def test_global_scope(self, bus: EventBus):
        """global 事件 target 保持 scope="global"."""
        bus.route_attach("client-a", "session-1")

        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.emit(
            DeliveredEvent(
                target=EventTarget(scope="global"),
            )
        )

        assert len(received) == 1
        assert received[0].target.scope == "global"
        assert received[0].target.client_ids == []


class TestEmitScopeClient:
    """scope="client"：投递者直接指定 client_ids，EventBus 不查路由表."""

    def test_client_scope(self, bus: EventBus):
        """client 事件直接指定 client_ids，EventBus 不修改."""
        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.emit(
            DeliveredEvent(
                target=EventTarget(scope="client", client_ids=["client-a", "client-b"]),
            )
        )

        assert len(received) == 1
        assert received[0].target.scope == "client"
        assert received[0].target.client_ids == ["client-a", "client-b"]


class TestEmitDefaultScope:
    """没有指定 target → fallback to scope="global"."""

    def test_no_target_defaults_to_global(self, bus: EventBus):
        """事件没有 target → EventBus 设为 scope="global"."""
        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.emit(DeliveredEvent())

        assert len(received) == 1
        assert received[0].target.scope == "global"


class TestContextvars:
    """contextvars auto-inject request_id / session_id."""

    def test_request_id_auto_inject(self, bus: EventBus):
        """set_request_context(request_id=...) 后，auto-inject request_id。"""
        received = []
        bus.subscribe(lambda e: received.append(e))

        token = set_request_context(request_id="req-123")
        bus.emit(DeliveredEvent())
        reset_request_context(token)

        assert received[0].request_id == "req-123"

    def test_session_id_auto_inject(self, bus: EventBus):
        """set_request_context(session_id=...) 后，auto-inject session_id。"""
        received = []
        bus.subscribe(lambda e: received.append(e))

        token = set_request_context(session_id="session-1")
        bus.emit(TextEvent(content="test"))
        reset_request_context(token)

        assert received[0].session_id == "session-1"

    def test_request_id_overrides(self, bus: EventBus):
        """RequestContext request_id 覆盖事件的 request_id。"""
        received = []
        bus.subscribe(lambda e: received.append(e))

        token = set_request_context(request_id="req-ctx")
        bus.emit(DeliveredEvent(request_id="req-explicit"))
        reset_request_context(token)

        assert received[0].request_id == "req-ctx"

    def test_no_context_keeps_event_request_id(self, bus: EventBus):
        """没有设置 RequestContext 时，事件 request_id 保持原值。"""
        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.emit(DeliveredEvent(request_id="req-explicit"))
        assert received[0].request_id == "req-explicit"


class TestSubscribeUnsubscribe:
    """subscribe / unsubscribe 管理 subscriber 列表."""

    def test_subscribe_and_receive(self, bus: EventBus):
        """subscribe 后收到事件."""
        received = []
        bus.subscribe(lambda e: received.append(e))
        bus.emit(DeliveredEvent())
        assert len(received) == 1

    def test_multiple_subscribers(self, bus: EventBus):
        """多个 subscriber 都收到事件."""
        received_a = []
        received_b = []
        bus.subscribe(lambda e: received_a.append(e))
        bus.subscribe(lambda e: received_b.append(e))
        bus.emit(DeliveredEvent())
        assert len(received_a) == 1
        assert len(received_b) == 1

    def test_unsubscribe_no_receive(self, bus: EventBus):
        """unsubscribe 后不再收到事件."""
        received = []

        def callback(e):
            received.append(e)

        bus.subscribe(callback)
        bus.unsubscribe(callback)
        bus.emit(DeliveredEvent())
        assert len(received) == 0

    def test_subscriber_error_does_not_block(self, bus: EventBus):
        """一个 subscriber 报错不阻断其他 subscriber."""
        received_good = []

        def bad_callback(e):
            raise ValueError("boom")

        bus.subscribe(bad_callback)
        bus.subscribe(lambda e: received_good.append(e))
        bus.emit(DeliveredEvent())
        assert len(received_good) == 1

    def test_subscriber_count(self, bus: EventBus):
        """subscriber_count 正确反映 subscriber 数量."""
        assert bus.subscriber_count == 0

        def callback1(e):
            pass

        def callback2(e):
            pass

        bus.subscribe(callback1)
        assert bus.subscriber_count == 1
        bus.subscribe(callback2)
        assert bus.subscriber_count == 2
        bus.unsubscribe(callback1)
        assert bus.subscriber_count == 1


class TestSessionScopeWithRoutingChanges:
    """路由表变更后，session scope 事件的目标随之变化."""

    def test_attach_after_first_emit(self, bus: EventBus):
        """新增订阅后，下一次 emit 包含新 client."""
        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.route_attach("client-a", "session-1")

        bus.emit(
            TextEvent(
                session_id="session-1",
                content="first",
                target=EventTarget(scope="session"),
            )
        )
        assert set(received[0].target.client_ids) == {"client-a"}

        bus.route_attach("client-b", "session-1")

        bus.emit(
            TextEvent(
                session_id="session-1",
                content="second",
                target=EventTarget(scope="session"),
            )
        )
        assert set(received[1].target.client_ids) == {"client-a", "client-b"}

    def test_detach_after_emit(self, bus: EventBus):
        """移除订阅后，下一次 emit 不包含该 client."""
        received = []
        bus.subscribe(lambda e: received.append(e))

        bus.route_attach("client-a", "session-1")
        bus.route_attach("client-b", "session-1")

        bus.emit(
            TextEvent(
                session_id="session-1",
                content="first",
                target=EventTarget(scope="session"),
            )
        )
        assert set(received[0].target.client_ids) == {"client-a", "client-b"}

        bus.route_detach("client-a", "session-1")

        bus.emit(
            TextEvent(
                session_id="session-1",
                content="second",
                target=EventTarget(scope="session"),
            )
        )
        assert set(received[1].target.client_ids) == {"client-b"}
