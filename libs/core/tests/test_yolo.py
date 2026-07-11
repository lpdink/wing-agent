"""Tests for yolo — agent-level yolo setting.

yolo 是 WingAgent 的简单 bool 属性：
  - 构造时从 template/override 获取，None 时 resolve 到全局 config
  - set_yolo() 直接修改 _yolo
  - 无 state bag、无优先级链
"""

from __future__ import annotations

from typing import Any
from unittest.mock import patch

import pytest

from wing.config import AgentConfig, Config, OpenAIConfig
from wing.event_bus import event_bus
from wing.gateway.protocol import AgentOverride


def _cfg(yolo: bool = False) -> Config:
    """Create a Config with specified global yolo."""
    return Config(
        openai=OpenAIConfig(base_url="https://api.example.com", api_key="test"),
        agents=[AgentConfig(name="default", model="gpt-4")],
        yolo=yolo,
    )


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


def _patch_agent_config(yolo: bool):
    """Patch get_config in agent.py (which imports it as a local reference)."""
    return patch("wing.agent.get_config", return_value=_cfg(yolo=yolo))


# ============================================================
# WingAgent.yolo property
# ============================================================


class TestYoloInit:
    """yolo 初始化测试。"""

    @pytest.mark.asyncio
    async def test_default_from_global_config_false(self, runtime: Any):
        """template yolo=None 时，从全局 config 获取（False）。"""
        with _patch_agent_config(yolo=False):
            session = runtime.create_session()
            assert session.agent.yolo is False

    @pytest.mark.asyncio
    async def test_default_from_global_config_true(self, runtime: Any):
        """template yolo=None 时，从全局 config 获取（True）。"""
        with _patch_agent_config(yolo=True):
            session = runtime.create_session()
            assert session.agent.yolo is True

    @pytest.mark.asyncio
    async def test_template_yolo_overrides_global(self, runtime: Any):
        """template yolo=True 覆盖全局 config yolo=False。"""
        template = runtime.template_manager.default
        template.yolo = True

        with _patch_agent_config(yolo=False):
            session = runtime.create_session()
            assert session.agent.yolo is True

    @pytest.mark.asyncio
    async def test_template_yolo_false_overrides_global_true(self, runtime: Any):
        """template yolo=False 覆盖全局 config yolo=True。"""
        template = runtime.template_manager.default
        template.yolo = False

        with _patch_agent_config(yolo=True):
            session = runtime.create_session()
            assert session.agent.yolo is False


# ============================================================
# AgentOverride yolo
# ============================================================


class TestYoloOverride:
    """AgentOverride.yolo 覆盖测试。"""

    @pytest.mark.asyncio
    async def test_override_yolo_true(self, runtime: Any):
        """AgentOverride.yolo=True 覆盖当前值。"""
        with _patch_agent_config(yolo=False):
            session = runtime.create_session()
            assert session.agent.yolo is False

            override = AgentOverride(yolo=True)
            session.apply_agent_override(override)

            assert session.agent.yolo is True

    @pytest.mark.asyncio
    async def test_override_yolo_false(self, runtime: Any):
        """AgentOverride.yolo=False 覆盖当前值。"""
        template = runtime.template_manager.default
        template.yolo = True

        session = runtime.create_session()
        assert session.agent.yolo is True

        override = AgentOverride(yolo=False)
        session.apply_agent_override(override)

        assert session.agent.yolo is False

    @pytest.mark.asyncio
    async def test_override_yolo_none_no_change(self, runtime: Any):
        """AgentOverride.yolo=None 不覆盖。"""
        template = runtime.template_manager.default
        template.yolo = True

        session = runtime.create_session()
        assert session.agent.yolo is True

        override = AgentOverride(yolo=None)
        session.apply_agent_override(override)

        assert session.agent.yolo is True


# ============================================================
# set_yolo
# ============================================================


class TestSetYolo:
    """agent.set_yolo() 测试。"""

    @pytest.mark.asyncio
    async def test_set_yolo_modifies_yolo(self, runtime: Any):
        """set_yolo 直接修改 _yolo。"""
        session = runtime.create_session()
        assert session.agent.yolo is False

        session.agent.set_yolo(True)
        assert session.agent.yolo is True
        assert session.agent._yolo is True

    @pytest.mark.asyncio
    async def test_set_yolo_toggle(self, runtime: Any):
        """set_yolo 可反复切换。"""
        session = runtime.create_session()

        session.agent.set_yolo(True)
        assert session.agent.yolo is True

        session.agent.set_yolo(False)
        assert session.agent.yolo is False

        session.agent.set_yolo(True)
        assert session.agent.yolo is True
