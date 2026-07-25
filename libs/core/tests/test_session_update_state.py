"""Session.update_state() 单元测试。

测试 Session 自身的状态变更逻辑：执行顺序、各分支是否正确调用。
使用真实的 Session 实例（通过 SessionManager 创建），而非 mock。
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from wing.session_manager import SessionManager


@pytest.fixture
def sm():
    """创建 SessionManager（memory 后端，不落盘）。"""
    from wing.store import MemorySessionStore

    return SessionManager({"memory": MemorySessionStore()}, default_backend="memory")


class TestSessionUpdateState:
    """Session.update_state() 各字段独立测试。"""

    @pytest.mark.asyncio
    async def test_update_model(self, sm):
        """更新模型名称。"""
        session = sm.create_session()
        original_model = session.agent.model
        assert original_model != "gpt-4o-mini"

        await session.update_state(model="gpt-4o-mini")
        assert session.agent.model == "gpt-4o-mini"

    @pytest.mark.asyncio
    async def test_update_title(self, sm):
        """设置 session 标题。"""
        session = sm.create_session()
        assert session.session_name is None

        await session.update_state(title="My Test Session")
        assert session.session_name == "My Test Session"

    @pytest.mark.asyncio
    async def test_update_thinking(self, sm):
        """开关 thinking 模式。"""
        session = sm.create_session()
        original = session.agent.model_provider.thinking

        await session.update_state(thinking=not original)
        assert session.agent.model_provider.thinking == (not original)

    @pytest.mark.asyncio
    async def test_update_reasoning_effort(self, sm):
        """设置推理力度。"""
        session = sm.create_session()
        await session.update_state(reasoning_effort="high")
        assert session.agent.model_provider.reasoning_effort == "high"

    @pytest.mark.asyncio
    async def test_update_yolo(self, sm):
        """开关 yolo 模式。"""
        session = sm.create_session()
        original = session.agent.yolo

        await session.update_state(yolo=not original)
        assert session.agent.yolo == (not original)

    @pytest.mark.asyncio
    async def test_none_params_skipped(self, sm):
        """所有参数为 None 时不执行任何操作。"""
        session = sm.create_session()
        original_model = session.agent.model
        original_name = session.session_name

        await session.update_state()

        assert session.agent.model == original_model
        assert session.session_name == original_name

    @pytest.mark.asyncio
    async def test_template_switch_called(self, sm):
        """template 不为 None 时调用 switch_template。"""
        session = sm.create_session()
        mock_template = MagicMock()
        session.switch_template = AsyncMock()

        await session.update_state(template=mock_template)
        session.switch_template.assert_awaited_once_with(mock_template)

    @pytest.mark.asyncio
    async def test_multiple_fields_updated(self, sm):
        """多字段同时更新，全部生效。"""
        session = sm.create_session()
        await session.update_state(
            model="gpt-4o-mini",
            title="Multi Update",
            thinking=True,
            reasoning_effort="medium",
            yolo=True,
        )

        assert session.agent.model == "gpt-4o-mini"
        assert session.session_name == "Multi Update"
        assert session.agent.model_provider.thinking is True
        assert session.agent.model_provider.reasoning_effort == "medium"
        assert session.agent.yolo is True
