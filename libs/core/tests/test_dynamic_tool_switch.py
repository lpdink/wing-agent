"""动态工具切换单元测试。

覆盖：
- 冷切换（空 chain）：CM 声明集同步更新，无 reminder
- 热切换（非空 chain）：CM 声明集冻结，reminder 写入 chain
- Reminder 内容格式：含 namespace、完整 schema、<tools> 标签
- Compaction 同步：CM 内部在 compact apply 后同步声明集
- set_tools 原子性：无效工具名时整体不切换
- GET /api/tools 端点（registry 层验证）
- POST /api/session/update tools 字段
"""

from __future__ import annotations

import pytest

from wing.event_bus import event_bus
from wing.schema import Message, Tool, ToolParam
from wing.tool_registry import tool_registry


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


def _make_tool(name: str, namespace: str = "default") -> Tool:
    """创建最小化 Tool 并注册到 registry。"""
    return Tool(
        name=name,
        namespace=namespace,
        description=f"Test tool: {name}",
        params=[ToolParam(name="input", type="string", description="input")],
        function=lambda _input="": f"{name}-result",
    )


@pytest.fixture
def register_test_tools():
    """注册测试工具到全局 registry，测试后清理。"""
    tools = [_make_tool("Alpha"), _make_tool("Beta"), _make_tool("Gamma")]
    for t in tools:
        tool_registry.register_tool(t)
    yield tools
    for t in tools:
        ns_map = tool_registry._namespaces.get(t.namespace, {})
        ns_map.pop(t.name, None)


@pytest.fixture
def register_remote_tools():
    """注册模拟远程工具。"""
    tools = [
        _make_tool("Bash", namespace="host-A"),
        _make_tool("Bash", namespace="host-B"),
    ]
    for t in tools:
        tool_registry.register_tool(t)
    yield tools
    for t in tools:
        ns_map = tool_registry._namespaces.get(t.namespace, {})
        ns_map.pop(t.name, None)


class TestColdSwitch:
    """冷切换：活跃链为空时 CM 声明集同步更新。"""

    @pytest.mark.asyncio
    async def test_cold_switch_syncs_declared(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        # 冷切换（空 chain）
        agent.set_tools(["Beta", "Gamma"])

        assert set(agent._tools.keys()) == {"Beta", "Gamma"}
        declared_names = {t.effective_llm_name for t in cm.declared_tools}
        assert declared_names == {"Beta", "Gamma"}

    @pytest.mark.asyncio
    async def test_cold_switch_no_reminder(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent

        agent.set_tools(["Beta"])

        chain = agent.context_manager.get_context_window()
        reminders = [m for m in chain if "[System Reminder]" in (m.content or "")]
        assert len(reminders) == 0


class TestHotSwitch:
    """热切换：活跃链有消息时 CM 冻结声明集，追加 reminder。"""

    @pytest.mark.asyncio
    async def test_hot_switch_freezes_declared(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        # 初始化声明集
        agent.set_tools(["Alpha"])
        # 添加消息使 chain 非空
        cm.add_message(Message(role="user", content="hello"))
        cm.add_message(Message(role="assistant", content="hi there"))

        agent.set_tools(["Beta"])

        # executable 变了
        assert set(agent._tools.keys()) == {"Beta"}
        # CM declared 冻结
        declared_names = {t.effective_llm_name for t in cm.declared_tools}
        assert declared_names == {"Alpha"}

    @pytest.mark.asyncio
    async def test_hot_switch_adds_reminder(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        agent.set_tools(["Alpha"])
        cm.add_message(Message(role="user", content="hello"))

        agent.set_tools(["Beta"])

        chain = cm.get_context_window()
        reminders = [m for m in chain if "[System Reminder]" in (m.content or "")]
        assert len(reminders) == 1
        assert reminders[0].role == "user"

    @pytest.mark.asyncio
    async def test_no_reminder_when_no_change(
        self, runtime, register_test_tools: list[Tool]
    ):
        """切换到相同工具集时不注入 reminder。"""
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        agent.set_tools(["Alpha"])
        cm.add_message(Message(role="user", content="hello"))

        # 切回相同集合
        agent.set_tools(["Alpha"])

        chain = cm.get_context_window()
        reminders = [m for m in chain if "[System Reminder]" in (m.content or "")]
        assert len(reminders) == 0


class TestReminderFormat:
    """Reminder 内容格式验证。"""

    @pytest.mark.asyncio
    async def test_reminder_contains_namespace(
        self, runtime, register_remote_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        agent.set_tools(["host-A.Bash"])
        cm.add_message(Message(role="user", content="hello"))

        agent.set_tools(["host-B.Bash"])

        chain = cm.get_context_window()
        reminder = next(m for m in chain if "[System Reminder]" in (m.content or ""))
        assert "host-A" in reminder.content
        assert "host-B" in reminder.content

    @pytest.mark.asyncio
    async def test_reminder_contains_full_schema(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        agent.set_tools(["Alpha"])
        cm.add_message(Message(role="user", content="hello"))

        agent.set_tools(["Beta"])

        chain = cm.get_context_window()
        reminder = next(m for m in chain if "[System Reminder]" in (m.content or ""))
        assert "<tools>" in reminder.content
        assert "</tools>" in reminder.content
        assert '"Beta"' in reminder.content
        assert '"parameters"' in reminder.content


class TestCompactionSync:
    """Compaction 后 CM 内部同步声明集。"""

    @pytest.mark.asyncio
    async def test_declared_syncs_on_manual_compact(
        self, runtime, register_test_tools: list[Tool]
    ):
        """do_manual_compact 后声明集同步为 current_tools。"""
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        # 设置初始工具并添加消息
        agent.set_tools(["Alpha"])
        cm.add_message(Message(role="user", content="hello " * 100))
        cm.add_message(Message(role="assistant", content="world " * 100))

        # 热切换（冻结 declared 为 Alpha）
        agent.set_tools(["Beta"])
        assert {t.effective_llm_name for t in cm.declared_tools} == {"Alpha"}

        # 手动 compact（需要 compactor 配置）——直接测试 _sync_declared_tools
        cm._sync_declared_tools(agent.tools)
        assert {t.effective_llm_name for t in cm.declared_tools} == {"Beta"}


class TestAtomicity:
    """set_tools 原子性。"""

    @pytest.mark.asyncio
    async def test_invalid_tool_name_no_switch(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent

        agent.set_tools(["Alpha"])

        with pytest.raises(ValueError, match="cannot resolve"):
            agent.set_tools(["Beta", "ghost.NonExistent"])

        # 整体不切换
        assert set(agent._tools.keys()) == {"Alpha"}


class TestToolsListAPI:
    """GET /api/tools 端点（registry 层验证）。"""

    def test_list_tools_returns_registered(self, register_test_tools: list[Tool]):
        _ = register_test_tools
        from wing.tool_registry import ToolRef

        tools = tool_registry.tools
        refs = [str(ToolRef(namespace=t.namespace, name=t.name)) for t in tools]
        assert "Alpha" in refs
        assert "Beta" in refs

    def test_list_tools_includes_namespace(self, register_remote_tools: list[Tool]):
        _ = register_remote_tools
        from wing.tool_registry import ToolRef

        tools = tool_registry.tools
        host_a_tools = [t for t in tools if t.namespace == "host-A"]
        assert len(host_a_tools) == 1
        assert str(ToolRef(namespace="host-A", name="Bash")) == "host-A.Bash"

    def test_list_tools_after_unregister(self, register_remote_tools: list[Tool]):
        _ = register_remote_tools
        tool_registry.unregister_namespace("host-A")
        tools = tool_registry.tools
        host_a_tools = [t for t in tools if t.namespace == "host-A"]
        assert len(host_a_tools) == 0


class TestUpdateSessionTools:
    """POST /api/session/update tools 字段。"""

    @pytest.mark.asyncio
    async def test_update_tools_triggers_switch(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()
        agent = session.agent

        await runtime.update_session(session_id=session.session_id, tools=["Beta"])

        assert set(agent._tools.keys()) == {"Beta"}

    @pytest.mark.asyncio
    async def test_update_tools_invalid_returns_error(
        self, runtime, register_test_tools: list[Tool]
    ):
        session = runtime.create_session()

        with pytest.raises(ValueError, match="cannot resolve"):
            await runtime.update_session(
                session_id=session.session_id, tools=["ghost.Tool"]
            )


class TestCreateSessionOverride:
    """创建 session 时 tools override（P0 回归验证）。"""

    @pytest.mark.asyncio
    async def test_create_with_tools_override_syncs_declared(
        self, runtime, register_test_tools: list[Tool]
    ):
        """创建时 override tools → 声明集与可执行集一致。"""
        from wing.gateway.protocol import AgentOverride

        session = runtime.create_session(agent_override=AgentOverride(tools=["Beta"]))
        agent = session.agent
        cm = agent.context_manager

        assert set(agent._tools.keys()) == {"Beta"}
        declared_names = {t.effective_llm_name for t in cm.declared_tools}
        assert declared_names == {"Beta"}
