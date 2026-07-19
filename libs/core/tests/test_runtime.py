"""Tests for WingRuntime — 核心最高抽象。

WingRuntime 组合 SessionManager + EventBus，提供统一入站入口。
测试围绕 contextvars 隔离、订阅管理、工具绑定隔离展开。

设计约束：
  - post() 是唯一入站入口，设置 contextvars，try/finally 确保恢复
  - create_session() 不涉及路由，subscribe() 封装路由注册 + 同步推送
  - SM._post() 不处理 contextvars（由 Runtime 统一管理）
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from wing.runtime import WingRuntime

from typing import Any

import pytest

from wing.event import DeliveredEvent
from wing.event_bus import event_bus
from wing.request_context import (
    reset_request_context,
    set_request_context,
)


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    """每个测试前后清理全局 EventBus，避免测试间干扰。

    EventBus 是全局单例，测试必须清理 subscribers 和 routing table。
    """
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


# ============================================================
# contextvars 隔离
# ============================================================


class TestContextvarsIsolation:
    """WingRuntime.post() 的 contextvars 管理。

    核心契约：
      1. post() 内 emit 的事件自动注入 request_id/session_id
      2. post() 返回后 contextvars 恢复原值
      3. 即使下游抛出异常，contextvars 仍恢复
    """

    @pytest.mark.asyncio
    async def test_events_during_post_carry_contextvars(self, runtime: Any):
        """post() 内 emit 的事件携带 Runtime 设置的 request_id/session_id。"""
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))

        session = runtime.create_session()
        event_bus.route_attach("test-client", session.session_id)

        await runtime.post(
            "hello",
            session_id=session.session_id,
            client_id="test-client",
            request_id="req-001",
        )

        # DeliveredEvent 应该自动注入 contextvars 的值
        delivered = [e for e in received if e.type == "delivered"]
        assert len(delivered) >= 1, "Should have at least one Delivered event"
        assert delivered[0].request_id == "req-001"
        assert delivered[0].session_id == session.session_id

    @pytest.mark.asyncio
    async def test_contextvars_restored_after_post(self, runtime: Any):
        """post() 返回后 contextvars 恢复到调用前的值。"""
        # 先在调用方设置 RequestContext
        token = set_request_context(
            request_id="outer-req",
            session_id="outer-sid",
            client_id="outer-cid",
        )

        session = runtime.create_session()
        event_bus.route_attach("test-client", session.session_id)
        await runtime.post(
            "hello",
            session_id=session.session_id,
            client_id="test-client",
            request_id="inner-req",
        )

        # post() 返回后，emit 一个事件验证 RequestContext 已恢复
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))
        event_bus.emit(DeliveredEvent())

        assert received[0].request_id == "outer-req"
        assert received[0].session_id == "outer-sid"

        # 清理
        reset_request_context(token)

    @pytest.mark.asyncio
    async def test_contextvars_restored_on_missing_session(self, runtime: Any):
        """post() 给不存在的 session_id 不会导致 contextvars 泄露。"""
        # 先设置外部 RequestContext
        token = set_request_context(request_id="outer")

        await runtime.post(
            "hello",
            session_id="non-existent-session",
            client_id="test-client",
        )

        # 恢复
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))
        event_bus.emit(DeliveredEvent())
        assert received[0].request_id == "outer"

        reset_request_context(token)

    @pytest.mark.asyncio
    async def test_multiple_posts_isolated(self, runtime: Any):
        """多次 post() 各自有自己的 contextvar，互不干扰。"""
        results: dict[str, list] = {"first": [], "second": []}
        session = runtime.create_session()
        event_bus.route_attach("test-client", session.session_id)

        # 第一次 post
        event_bus.subscribe(lambda e: results["first"].append(e))
        await runtime.post(
            "first",
            session_id=session.session_id,
            client_id="test-client",
            request_id="req-1",
        )

        # 清空 subscriber 后第二次 post
        event_bus._subscribers.clear()
        event_bus.subscribe(lambda e: results["second"].append(e))
        await runtime.post(
            "second",
            session_id=session.session_id,
            client_id="test-client",
            request_id="req-2",
        )

        first_delivered = [e for e in results["first"] if e.type == "delivered"]
        second_delivered = [e for e in results["second"] if e.type == "delivered"]

        assert first_delivered[0].request_id == "req-1"
        assert second_delivered[0].request_id == "req-2"


# ============================================================
# 订阅管理（subscribe / unsubscribe）
# ============================================================


class TestSubscription:
    """WingRuntime 的订阅管理。

    核心契约：
      - subscribe(client_id, session_id) 注册路由 + 推送 SyncSessionEvent + ContextStatsEvent
      - unsubscribe(client_id, session_id) 取消路由注册
      - subscribe 不存在的 session 抛出 LookupError
    """

    @pytest.mark.asyncio
    async def test_subscribe_attaches_route(self, runtime: Any):
        """subscribe 注册路由。"""
        session = runtime.create_session()
        runtime.subscribe("tui", session.session_id)
        assert "tui" in event_bus.routing_table
        assert session.session_id in event_bus.routing_table["tui"]

    @pytest.mark.asyncio
    async def test_subscribe_pushes_sync_events(self, runtime: Any):
        """subscribe 推送 SyncSessionEvent + ContextStatsEvent。"""
        session = runtime.create_session()
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))

        runtime.subscribe("client-1", session.session_id)

        sync_events = [e for e in received if e.type == "sync_session"]
        stats_events = [e for e in received if e.type == "context_stats"]
        assert len(sync_events) == 1
        assert len(stats_events) == 1
        assert sync_events[0].session_id == session.session_id

    @pytest.mark.asyncio
    async def test_sync_session_event_carries_config_fields(self, runtime: Any):
        """SyncSessionEvent 携带 model/thinking/reasoning_effort/yolo 字段。"""
        session = runtime.create_session()
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))

        runtime.subscribe("client-1", session.session_id)

        sync_events = [e for e in received if e.type == "sync_session"]
        assert len(sync_events) == 1
        evt = sync_events[0]
        # 新字段应反映 session agent 的当前配置
        assert evt.model == session.agent.model
        assert evt.thinking == session.agent.model_provider.thinking
        assert evt.reasoning_effort == session.agent.model_provider.reasoning_effort
        assert evt.yolo == session.agent.yolo

    @pytest.mark.asyncio
    async def test_subscribe_nonexistent_session_raises(self, runtime: Any):
        """subscribe 不存在的 session 抛出 LookupError。"""
        with pytest.raises(LookupError, match="Session not found"):
            runtime.subscribe("client-1", "nonexistent")

    @pytest.mark.asyncio
    async def test_unsubscribe_detaches_route(self, runtime: Any):
        """unsubscribe 取消路由注册。"""
        session = runtime.create_session()
        runtime.subscribe("tui", session.session_id)
        assert "tui" in event_bus.routing_table

        runtime.unsubscribe("tui", session.session_id)
        assert (
            "tui" not in event_bus.routing_table
            or session.session_id not in event_bus.routing_table.get("tui", set())
        )

    @pytest.mark.asyncio
    async def test_create_session_no_routing(self, runtime: Any):
        """create_session 不涉及路由。"""
        runtime.create_session()
        assert event_bus.routing_table == {}

    @pytest.mark.asyncio
    async def test_multiple_clients_subscribe(self, runtime: Any):
        """多个 client 订阅同一个 session。"""
        session = runtime.create_session()
        runtime.subscribe("client-a", session.session_id)
        runtime.subscribe("client-b", session.session_id)
        assert "client-a" in event_bus.routing_table
        assert "client-b" in event_bus.routing_table
        assert session.session_id in event_bus.routing_table["client-a"]
        assert session.session_id in event_bus.routing_table["client-b"]


# ============================================================
# Session 生命周期
# ============================================================


class TestSessionLifecycle:
    """WingRuntime 的 session 生命周期。

    核心契约：
      - create_session 不涉及路由
      - resume_session 从磁盘恢复，内存中已存在时直接返回
      - fork_session 失败时 raise LookupError
    """

    @pytest.mark.asyncio
    async def test_fork_session_raises_on_nonexistent_source(self, runtime: Any):
        """fork_session source 不存在时 raise LookupError。"""
        session = runtime.create_session()
        with pytest.raises(LookupError, match="Fork failed"):
            runtime.fork_session(session.session_id, "nonexistent-uuid")

    @pytest.mark.asyncio
    async def test_resume_session_returns_existing(self, runtime: Any):
        """resume_session 内存中已存在时直接返回。"""
        session = runtime.create_session()
        resumed = runtime.resume_session(session.session_id)
        assert resumed is session

    @pytest.mark.asyncio
    async def test_resume_session_nonexistent_raises(self, runtime: Any):
        """resume_session session 不存在时 raise LookupError。"""
        with pytest.raises(LookupError, match="Session not found"):
            runtime.resume_session("definitely-not-exists")

    @pytest.mark.asyncio
    async def test_real_fork_creates_new_session(self, runtime: Any):
        """真实 fork 操作创建新 session。"""
        from wing.schema import Message

        session = runtime.create_session()

        # 添加用户消息
        session.agent.context_manager.add_message(
            Message(role="user", content="test message for forking")
        )

        # 获取 branch targets
        targets = session.agent.context_manager.get_branch_targets()
        assert len(targets) > 0, "Should have at least one branch target"

        # Fork at first target
        new_session, draft = runtime.fork_session(
            session.session_id,
            targets[0]["uuid"],
        )
        assert new_session is not None
        assert new_session.session_id != session.session_id

    @pytest.mark.asyncio
    async def test_resume_session_from_disk(self, runtime: Any):
        """resume_session 从磁盘恢复不在内存中的 session。"""
        from wing.schema import Message

        # 创建并持久化一个 session
        old_session = runtime.create_session()
        old_session.agent.context_manager.add_message(
            Message(role="user", content="test for resume")
        )
        old_sid = old_session.session_id

        # 从内存中移除
        del runtime.sm._sessions[old_sid]

        # resume 应该从磁盘恢复
        resumed = runtime.resume_session(old_sid)
        assert resumed.session_id == old_sid
        assert resumed is not old_session


# ============================================================
# 查询接口
# ============================================================
# Session 序列化方法
# ============================================================


class TestSessionSerialization:
    """Session.to_agent_info() 和 Session.serialize_messages() 的独立测试。"""

    @pytest.mark.asyncio
    async def test_to_agent_info_returns_correct_fields(self, runtime: Any):
        """to_agent_info 返回包含 model_name、tools、skills、rules、workspace 的 AgentInfo。"""
        session = runtime.create_session()
        info = session.to_agent_info()
        assert info.model_name is not None
        assert isinstance(info.tools, list)
        assert isinstance(info.skills, list)
        assert isinstance(info.rules, list)

    @pytest.mark.asyncio
    async def test_serialize_messages_empty(self, runtime: Any):
        """空消息历史序列化为空列表。"""
        session = runtime.create_session()
        msgs = session.serialize_messages()
        # 新 session 可能只有 system prompt 在 context window 中
        # active_chain 无用户消息时为空
        assert isinstance(msgs, list)
        for m in msgs:
            assert "role" in m
            assert "content" in m
            assert "uuid" in m

    @pytest.mark.asyncio
    async def test_serialize_messages_with_user_message(self, runtime: Any):
        """添加消息后序列化包含正确字段。"""
        from wing.schema import Message

        session = runtime.create_session()
        session.agent.context_manager.add_message(Message(role="user", content="hello"))
        msgs = session.serialize_messages()
        user_msgs = [m for m in msgs if m["role"] == "user"]
        assert len(user_msgs) >= 1
        assert user_msgs[0]["content"] == "hello"


# ============================================================


class TestQueryInterface:
    """WingRuntime 的查询接口。"""

    @pytest.mark.asyncio
    async def test_get_session_state_existing(self, runtime: Any):
        """get_session_state 返回存在的 session 状态。"""
        session = runtime.create_session()
        state = runtime.get_session_state(session.session_id)
        assert state is not None
        assert state["session_id"] == session.session_id
        assert "messages" in state
        assert "agent" in state

    @pytest.mark.asyncio
    async def test_get_session_state_nonexistent(self, runtime: Any):
        """get_session_state 不存在的 session 返回 None。"""
        assert runtime.get_session_state("nonexistent") is None

    @pytest.mark.asyncio
    async def test_list_sessions(self, runtime: Any):
        """list_sessions 返回 SessionInfo 列表。"""
        # 不创建任何 session，列表可能为空（取决于磁盘状态）
        result = runtime.list_sessions()
        assert isinstance(result, list)


# ============================================================
# 工具绑定隔离
# ============================================================


class TestToolBindingIsolation:
    """fork/resume 后，新 agent 的工具闭包不能引用旧 agent。

    之前 bug：fork_session / switch_session 传入的 tools=source.agent.tools
    是已经绑定过的（inject_agent_param=None），_bind_tools 直接复用，
    导致 bash 工具的 wrapper 闭包 _agent 仍指向旧 agent。
    """

    @pytest.mark.asyncio
    async def test_fork_session_fork_tools_bound_to_new_agent(
        self, runtime: WingRuntime
    ):
        """fork 后，新 agent 的工具闭包引用新 agent 而非旧 agent。"""
        from wing.schema import Message

        old_session = runtime.create_session()
        old_agent_id = id(old_session.agent)

        # 添加消息，让 fork 有目标
        old_session.agent.context_manager.add_message(
            Message(role="user", content="test for fork")
        )
        targets = old_session.agent.context_manager.get_branch_targets()
        assert len(targets) > 0

        new_session, _ = runtime.fork_session(
            old_session.session_id,
            targets[0]["uuid"],
        )
        new_agent = new_session.agent
        new_agent_id = id(new_agent)

        # 新旧 agent 不同
        assert old_agent_id != new_agent_id

        # 验证新 agent 的 bash 工具闭包绑定到新 agent 而非旧 agent
        bash_tool = new_agent._tool_map["Bash"]
        # wrapper 的 __kwdefaults__ 包含 _agent=self 的默认值
        captured_agent = bash_tool.function.__kwdefaults__["_agent"]  # ty: ignore[unresolved-attribute]
        assert id(captured_agent) == new_agent_id, (
            f"bash 工具仍绑定旧 agent ({old_agent_id})，应为新 agent ({new_agent_id})"
        )

    @pytest.mark.asyncio
    async def test_resume_session_tools_bound_to_new_agent(self, runtime: WingRuntime):
        """resume_session 后，新 agent 的工具闭包引用新 agent 而非旧 agent。

        模拟 /ss 恢复场景：从会话列表选中一个历史 session（仅在磁盘上，
        不在内存中），通过 resume_session 创建新 agent。
        """
        from wing.schema import Message

        # 先建一个"历史 session"并持久化到磁盘
        old_session = runtime.create_session()
        old_agent_id = id(old_session.agent)
        old_session.agent.context_manager.add_message(
            Message(role="user", content="test for resume")
        )
        old_sid = old_session.session_id

        # 从内存中移除——模拟它属于另一个 runtime 实例（仅磁盘上有文件）
        del runtime.sm._sessions[old_sid]

        # resume 到历史 session（此时它不在内存中，会触发磁盘恢复）
        new_session = runtime.resume_session(old_sid)
        new_agent = new_session.agent
        new_agent_id = id(new_agent)

        # 从磁盘重建的 agent 是全新对象
        assert old_agent_id != new_agent_id

        # 验证新 agent 的 bash 工具闭包绑定到新 agent
        bash_tool = new_agent._tool_map["Bash"]
        captured_agent = bash_tool.function.__kwdefaults__["_agent"]  # ty: ignore[unresolved-attribute]
        assert id(captured_agent) == new_agent_id, (
            f"bash 工具仍绑定旧 agent ({old_agent_id})，应为新 agent ({new_agent_id})"
        )


# ============================================================
# post() 路由行为
# ============================================================


class TestPostRouting:
    """WingRuntime.post() 的正确路由。

    核心契约：
      - post() 委托给 SM._post()
      - session_id 必填（无 session 时抛错或日志）
      - / 开头的魔术命令正确路由
    """

    @pytest.mark.asyncio
    async def test_post_delegates_to_sm(self, runtime: Any):
        """post() 调用 SM._post() 处理消息。"""
        session = runtime.create_session()
        event_bus.route_attach("test-client", session.session_id)
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))

        await runtime.post(
            "plain message",
            session_id=session.session_id,
            client_id="test-client",
        )

        # 应该有 DeliveredEvent（总是 emit）
        delivered = [e for e in received if e.type == "delivered"]
        assert len(delivered) >= 1

    @pytest.mark.asyncio
    async def test_post_unrecognized_command_falls_to_agent(self, runtime: Any):
        """不认识的 /command 传给 agent 处理。"""
        session = runtime.create_session()
        event_bus.route_attach("test-client", session.session_id)
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))

        # /nonexistent 不是注册的魔术命令
        await runtime.post(
            "/nonexistent",
            session_id=session.session_id,
            client_id="test-client",
        )

        # agent 会处理并产生事件——主要验证不抛异常
        assert True

    @pytest.mark.asyncio
    async def test_session_not_found_logs_error(self, runtime: Any):
        """不存在的 session_id 不抛异常，只是 log error。"""
        # 这个方法应该不会抛异常
        await runtime.post(
            "hello",
            session_id="definitely-not-exists",
            client_id="test-client",
        )
        assert True


# ============================================================
# SM._post() 不再处理 contextvars
# ============================================================


class TestSMPostNoContextvars:
    """SM._post() 不设置/恢复 contextvars——由 WingRuntime 统一管理。

    注意：这个测试需要 SM._post() 存在。在重构过程中，如果直接调 SM._post()
    而不经 WingRuntime，事件应该没有 contextvar 注入。
    """

    @pytest.mark.asyncio
    async def test_sm_post_does_not_set_contextvars(self, runtime: Any):
        """直接调 SM._post()（不经 Runtime）时，事件不自动注入 contextvars。

        SM._post() 不再管理 contextvars——request_id 参数仅透传给 agent，
        不会被注入到事件的 request_id 字段。
        """
        received: list = []
        event_bus.subscribe(lambda e: received.append(e))

        # 直接调 SM._post()——不经过 WingRuntime
        session = runtime.create_session()
        event_bus.route_attach("test-client", session.session_id)
        await runtime.sm._post(
            "hello",
            session_id=session.session_id,
            client_id="test-client",
            request_id="direct-call",
        )

        # DeliveredEvent 的 request_id 应为默认 UUID
        # 因为 SM._post() 不设 contextvar，EventBus 不覆盖
        delivered = [e for e in received if e.type == "delivered"]
        assert len(delivered) >= 1
        assert delivered[0].request_id != "direct-call"
