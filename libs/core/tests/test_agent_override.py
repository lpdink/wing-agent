"""Tests for AgentOverride — session 创建时的 agent 参数覆盖。

覆盖 Session.apply_agent_override() 的行为：
  - model 覆盖
  - system_prompt 替换 / 追加
  - tools 替换
  - max_turns 设置
  - effort (reasoning_effort) 设置
  - None 字段不覆盖

同时测试 WingAgent 的 setter 方法：
  - replace_tools()
  - set_max_turns()
  - set_reasoning_effort()
"""

from __future__ import annotations

from typing import Any

import pytest

from wing.event_bus import event_bus
from wing.gateway.protocol import AgentOverride


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


# ============================================================
# Session.apply_agent_override()
# ============================================================


class TestApplyAgentOverride:
    """Session.apply_agent_override() 行为测试。"""

    @pytest.mark.asyncio
    async def test_override_model(self, runtime: Any):
        """model 覆盖直接修改 agent.model。"""
        session = runtime.create_session()
        original_model = session.agent.model

        override = AgentOverride(model="gpt-4o-mini")
        session.apply_agent_override(override)

        assert session.agent.model == "gpt-4o-mini"
        assert session.agent.model != original_model

    @pytest.mark.asyncio
    async def test_override_system_prompt_replace(self, runtime: Any):
        """system_prompt 替换 setin_system_prompt。"""
        session = runtime.create_session()
        original = session.context_manager.setin_system_prompt

        override = AgentOverride(system_prompt="You are a coding assistant.")
        session.apply_agent_override(override)

        assert (
            session.context_manager.setin_system_prompt == "You are a coding assistant."
        )
        assert session.context_manager.setin_system_prompt != original

    @pytest.mark.asyncio
    async def test_override_system_prompt_append(self, runtime: Any):
        """append_system_prompt 追加到 setin_system_prompt 末尾。"""
        session = runtime.create_session()

        # 先设置一个非空的 system_prompt 以便测试追加
        session.context_manager.setin_system_prompt = "Original prompt."

        override = AgentOverride(append_system_prompt="Always respond in Chinese.")
        session.apply_agent_override(override)

        # 实现用 "\n" 连接，所以结果是 "Original prompt.\n" + "Always..."
        assert (
            session.context_manager.setin_system_prompt
            == "Original prompt.\nAlways respond in Chinese."
        )

    @pytest.mark.asyncio
    async def test_override_replace_then_append(self, runtime: Any):
        """system_prompt + append_system_prompt 同时存在时，先替换再追加。"""
        session = runtime.create_session()

        override = AgentOverride(
            system_prompt="Base prompt.",
            append_system_prompt=" Extra instruction.",
        )
        session.apply_agent_override(override)

        assert (
            session.context_manager.setin_system_prompt
            == "Base prompt.\n Extra instruction."
        )

    @pytest.mark.asyncio
    async def test_override_tools(self, runtime: Any):
        """tools 替换（从 tool_registry 获取新工具）。"""
        session = runtime.create_session()
        original_tool_names = [t.name for t in session.agent.tools]

        # 只保留 Read 工具（假设 default template 有 Read）
        if "Read" in original_tool_names:
            override = AgentOverride(tools=["Read"])
            session.apply_agent_override(override)

            new_tool_names = [t.name for t in session.agent.tools]
            assert new_tool_names == ["Read"]
        else:
            pytest.skip("Default template does not have 'Read' tool")

    @pytest.mark.asyncio
    async def test_override_max_turns(self, runtime: Any):
        """max_turns 覆盖。"""
        session = runtime.create_session()
        # Default should be from template (likely None)
        assert session.agent.max_turns is None

        override = AgentOverride(max_turns=50)
        session.apply_agent_override(override)

        assert session.agent.max_turns == 50

    @pytest.mark.asyncio
    async def test_override_effort(self, runtime: Any):
        """effort (reasoning_effort) 覆盖。"""
        session = runtime.create_session()

        override = AgentOverride(effort="high")
        session.apply_agent_override(override)

        assert session.agent.model_provider.reasoning_effort == "high"

    @pytest.mark.asyncio
    async def test_none_fields_not_overridden(self, runtime: Any):
        """所有字段为 None 时不修改任何值。"""
        session = runtime.create_session()
        original_model = session.agent.model
        original_prompt = session.context_manager.setin_system_prompt
        original_tools = [t.name for t in session.agent.tools]
        original_max_turns = session.agent.max_turns
        original_effort = session.agent.model_provider.reasoning_effort

        override = AgentOverride()  # All None
        session.apply_agent_override(override)

        assert session.agent.model == original_model
        assert session.context_manager.setin_system_prompt == original_prompt
        assert [t.name for t in session.agent.tools] == original_tools
        assert session.agent.max_turns == original_max_turns
        assert session.agent.model_provider.reasoning_effort == original_effort

    @pytest.mark.asyncio
    async def test_override_via_create_session(self, runtime: Any):
        """通过 runtime.create_session() 传入 agent_override。"""
        override = AgentOverride(
            model="gpt-4o",
            max_turns=25,
            effort="low",
        )
        session = runtime.create_session(agent_override=override)

        assert session.agent.model == "gpt-4o"
        assert session.agent.max_turns == 25
        assert session.agent.model_provider.reasoning_effort == "low"


# ============================================================
# WingAgent setter methods
# ============================================================


class TestWingAgentSetters:
    """WingAgent 的公共 setter 方法测试。"""

    @pytest.mark.asyncio
    async def test_replace_tools_valid(self, runtime: Any):
        """replace_tools 替换为新的工具列表。"""
        session = runtime.create_session()
        agent = session.agent

        original_names = [t.name for t in agent.tools]
        if len(original_names) >= 2:
            # 只保留第一个工具
            agent.replace_tools([original_names[0]])
            new_names = [t.name for t in agent.tools]
            assert new_names == [original_names[0]]

    @pytest.mark.asyncio
    async def test_replace_tools_empty(self, runtime: Any):
        """replace_tools 空列表清除所有工具。"""
        session = runtime.create_session()
        agent = session.agent

        agent.replace_tools([])
        assert agent.tools == []

    @pytest.mark.asyncio
    async def test_replace_tools_unknown_ignored(self, runtime: Any):
        """replace_tools 忽略不存在的工具名。"""
        session = runtime.create_session()
        agent = session.agent

        agent.replace_tools(["NonExistentTool123"])
        assert agent.tools == []

    @pytest.mark.asyncio
    async def test_set_max_turns(self, runtime: Any):
        """set_max_turns 设置和清除。"""
        session = runtime.create_session()
        agent = session.agent

        agent.set_max_turns(100)
        assert agent.max_turns == 100

        agent.set_max_turns(None)
        assert agent.max_turns is None

    @pytest.mark.asyncio
    async def test_set_reasoning_effort(self, runtime: Any):
        """set_reasoning_effort 设置 effort 级别。"""
        session = runtime.create_session()
        agent = session.agent

        agent.set_reasoning_effort("low")
        assert agent.model_provider.reasoning_effort == "low"

        agent.set_reasoning_effort(None)
        assert agent.model_provider.reasoning_effort is None

    @pytest.mark.asyncio
    async def test_bind_tools_llm_name_collision_raises(self, runtime: Any):
        """不同 namespace 的工具若 effective_llm_name 相同，绑定时 raise。"""
        from wing.schema import Tool

        session = runtime.create_session()
        agent = session.agent

        t1 = Tool(
            name="Bash",
            namespace="client-a",
            description="",
            params=[],
            function=lambda: None,
        )
        t2 = Tool(
            name="Bash",
            namespace="client-b",
            description="",
            params=[],
            function=lambda: None,
        )
        with pytest.raises(ValueError, match="LLM name collision"):
            agent._bind_tools([t1, t2])

    @pytest.mark.asyncio
    async def test_bind_tools_different_llm_names_ok(self, runtime: Any):
        """不同 namespace 的工具若 llm_name 不同，绑定正常。"""
        from wing.schema import Tool

        session = runtime.create_session()
        agent = session.agent

        t1 = Tool(
            name="Bash",
            namespace="client-a",
            llm_name="BashA",
            description="",
            params=[],
            function=lambda: None,
        )
        t2 = Tool(
            name="Bash",
            namespace="client-b",
            llm_name="BashB",
            description="",
            params=[],
            function=lambda: None,
        )
        bound = agent._bind_tools([t1, t2])
        assert "BashA" in bound
        assert "BashB" in bound


# ============================================================
# max_turns config integration
# ============================================================


class TestMaxTurnsConfig:
    """AgentConfig.max_turns 通过 template 传递到 WingAgent。"""

    def test_agent_config_max_turns_default_none(self):
        """AgentConfig.max_turns 默认 None。"""
        from wing.config import AgentConfig

        config = AgentConfig(name="test", model="gpt-4")
        assert config.max_turns is None

    def test_agent_config_max_turns_set(self):
        """AgentConfig.max_turns 可设置。"""
        from wing.config import AgentConfig

        config = AgentConfig(name="test", model="gpt-4", max_turns=50)
        assert config.max_turns == 50

    def test_agent_template_from_config_max_turns(self):
        """AgentTemplate.from_config 传递 max_turns。"""
        from wing.agent_template import AgentTemplate
        from wing.config import AgentConfig

        config = AgentConfig(name="test", model="gpt-4", max_turns=30)
        template = AgentTemplate.from_config(config)
        assert template.max_turns == 30

    def test_agent_template_from_config_default_none(self):
        """AgentTemplate.from_config 默认 max_turns 为 None。"""
        from wing.agent_template import AgentTemplate
        from wing.config import AgentConfig

        config = AgentConfig(name="test", model="gpt-4")
        template = AgentTemplate.from_config(config)
        assert template.max_turns is None
