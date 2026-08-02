"""WingRuntime session 操作方法的单元测试。

测试 Runtime 层的业务逻辑（尤其是事件发射），
而不是 route handler 的胶水代码。
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from wing.event import SessionStateChangedEvent
from wing.event_bus import event_bus


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    rt = WingRuntime()
    yield rt
    # cleanup


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    """隔离 EventBus——每个测试独立收集事件。"""
    # 先清理所有之前的订阅
    event_bus._subscribers.clear()
    received: list = []

    def collector(e):
        received.append(e)

    event_bus.subscribe(collector)
    yield received
    try:
        event_bus.unsubscribe(collector)
    except Exception:
        pass


@pytest.fixture
def mock_session():
    """创建带完整 mock 属性的 session。"""
    session = MagicMock()
    session.session_id = "test-session"
    session.session_name = "Test Session"
    session.template_name = "default"
    session.agent.model = "gpt-4o"
    session.agent.yolo = False
    session.agent.model_provider.thinking = False
    session.agent.model_provider.reasoning_effort = None
    session.update_state = AsyncMock()
    return session


class TestRuntimeUpdateSessionEvents:
    """测试 Runtime.update_session() 的事件发射逻辑。

    这些是之前从 gateway HTTP 测试中删除的关键测试——
    现在在 Runtime 层覆盖。
    """

    @pytest.mark.asyncio
    async def test_update_model_emits_event_with_none_for_unchanged_fields(
        self, runtime, mock_session, cleanup_event_bus
    ):
        """只改 model 时，event 中其他字段为 None。"""
        runtime.sm._sessions["test-session"] = mock_session
        mock_session.agent.model = "gpt-4o-mini"

        await runtime.update_session("test-session", model="gpt-4o-mini")

        events = [
            e for e in cleanup_event_bus if isinstance(e, SessionStateChangedEvent)
        ]
        assert len(events) == 1
        event = events[0]
        assert event.model == "gpt-4o-mini"
        assert event.thinking is None
        assert event.yolo is None
        assert event.title is None
        assert event.agent is None
        assert event.target is not None
        # EventBus 将 scope="session" 转换为 scope="client" + 计算的 client_ids
        # 这里无客户端订阅，所以 client_ids 为空
        assert event.target.scope == "client"
        assert event.target.client_ids == []

    @pytest.mark.asyncio
    async def test_update_multi_fields_emits_all_changes(
        self, runtime, mock_session, cleanup_event_bus
    ):
        """多字段同时更新时 event 携带所有变更字段。"""
        runtime.sm._sessions["test-session"] = mock_session

        # mock template_manager for agent switch
        mock_template = MagicMock()
        runtime.sm._template_manager = MagicMock()
        runtime.sm._template_manager.get.return_value = mock_template

        # simulate post-update state
        mock_session.agent.model = "gpt-4o-mini"
        mock_session.session_name = "new title"
        mock_session.template_name = "coder"
        mock_session.agent.model_provider.thinking = True
        mock_session.agent.yolo = True

        await runtime.update_session(
            "test-session",
            agent="coder",
            model="gpt-4o-mini",
            title="new title",
            thinking=True,
            yolo=True,
        )

        events = [
            e for e in cleanup_event_bus if isinstance(e, SessionStateChangedEvent)
        ]
        assert len(events) == 1
        event = events[0]
        assert event.model == "gpt-4o-mini"
        assert event.thinking is True
        assert event.yolo is True
        assert event.title == "new title"
        assert event.agent == "coder"

    @pytest.mark.asyncio
    async def test_update_agent_emits_reset_state_fields(
        self, runtime, mock_session, cleanup_event_bus
    ):
        """agent 切换后 event 携带 switch_template 重置后的 thinking/reasoning_effort/yolo。

        这是最容易出错的边界情况：切换 template 后 agent 的状态可能被
        template 默认值覆盖，event 需要报告覆盖后的值而非 None。
        """
        runtime.sm._sessions["test-session"] = mock_session

        # 模拟 switch_template 后的 agent 状态
        mock_session.agent.model_provider.thinking = True
        mock_session.agent.model_provider.reasoning_effort = "medium"
        mock_session.agent.yolo = False
        mock_session.agent.model = "gpt-4o"
        mock_session.template_name = "coder"

        # mock template_manager
        mock_template = MagicMock()
        runtime.sm._template_manager = MagicMock()
        runtime.sm._template_manager.get.return_value = mock_template

        await runtime.update_session("test-session", agent="coder")

        events = [
            e for e in cleanup_event_bus if isinstance(e, SessionStateChangedEvent)
        ]
        assert len(events) == 1
        event = events[0]
        assert event.agent == "coder"
        assert event.model == "gpt-4o"
        # agent switch 应报告重置后的值
        assert event.thinking is True
        assert event.reasoning_effort == "medium"
        assert event.yolo is False
        assert event.title is None

    @pytest.mark.asyncio
    async def test_update_session_not_found_raises_lookup_error(self, runtime):
        """session 不存在时 raise LookupError（route 映射到 404）。"""
        with pytest.raises(LookupError, match="Session not found"):
            await runtime.update_session("nonexistent", model="gpt-4o")

    @pytest.mark.asyncio
    async def test_update_agent_template_not_found_raises_lookup_error(
        self, runtime, mock_session
    ):
        """template 不存在时 raise LookupError（route 映射到 404）。"""
        runtime.sm._sessions["test-session"] = mock_session
        runtime.sm._template_manager = MagicMock()
        runtime.sm._template_manager.get.return_value = None
        runtime.sm._template_manager.all_names = ["default", "coder"]

        with pytest.raises(LookupError, match="template 'nonexistent' not found"):
            await runtime.update_session("test-session", agent="nonexistent")

    @pytest.mark.asyncio
    async def test_all_events_routed_to_session_subscribers(
        self, runtime, mock_session, cleanup_event_bus
    ):
        """所有通过 Runtime 发射的事件都经过 EventBus 的 session→client 路由。

        EventBus 将 scope="session" 转换为 scope="client" + 计算的 client_ids。
        验证事件不是 scope="global"（那意味着 _emit_session_event 没生效）。
        """
        runtime.sm._sessions["test-session"] = mock_session

        await runtime.update_session("test-session", model="gpt-4o-mini")

        for event in cleanup_event_bus:
            if event.target is not None:
                assert event.target.scope != "global", (
                    f"Event {type(event).__name__} has scope='global' — "
                    f"_emit_session_event should have set scope='session'"
                )


class TestAnthropicThinkingRejection:
    """Anthropic 协议的 thinking 运行时更新拒绝（thinking 由 extra_body 派生）。"""

    @pytest.mark.asyncio
    async def test_anthropic_thinking_update_rejected(self, runtime, mock_session):
        mock_session.agent.model_provider.protocol = "anthropic"
        runtime.sm._sessions["test-session"] = mock_session

        with pytest.raises(ValueError, match="extra_body"):
            await runtime.update_session("test-session", thinking=True)
        mock_session.update_state.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_openai_thinking_update_allowed(self, runtime, mock_session):
        mock_session.agent.model_provider.protocol = "openai"
        runtime.sm._sessions["test-session"] = mock_session

        await runtime.update_session("test-session", thinking=True)
        mock_session.update_state.assert_awaited_once()


class TestReloadProviderIsolation:
    """reload_system 的 provider 驱逐重建：单 session 失败不阻断其余。"""

    @pytest.mark.asyncio
    async def test_one_bad_session_does_not_skip_rest(self, runtime, monkeypatch):
        from wing.config import get_config

        s1 = runtime.create_session()
        s2 = runtime.create_session()

        s1.agent.rebuild_providers = AsyncMock(side_effect=RuntimeError("boom"))
        s2.agent.rebuild_providers = AsyncMock()
        monkeypatch.setattr("wing.provider.reset_registry", AsyncMock())
        # 隔离环境配置：CI 无用户 config 文件，load_config(reload=True) 会因
        # 文件缺失提前中止 reload（本测试只关心 provider 重建的失败隔离）。
        monkeypatch.setattr(
            "wing.config.load_config", lambda reload=False: get_config()
        )

        result = await runtime.reload_system()

        provider_item = next(i for i in result.items if i.name == "provider")
        assert provider_item.ok is False
        assert "rebuilt 1 session(s)" in provider_item.detail
        assert "boom" in provider_item.detail
        # 坏 session 之后的 session 仍然被重建（不被跳过）
        s2.agent.rebuild_providers.assert_awaited_once()
