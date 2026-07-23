"""Tests for ToolRegistry namespace support and ToolRef parsing."""

import pytest

from wing.tool_registry import DEFAULT_NAMESPACE, ToolRef, ToolRegistry


# ============================================================
# ToolRef 解析
# ============================================================


class TestToolRef:
    """ToolRef.parse() 和 __str__ 测试。"""

    def test_bare_name_defaults_to_default_namespace(self):
        ref = ToolRef.parse("Bash")
        assert ref.namespace == DEFAULT_NAMESPACE
        assert ref.name == "Bash"

    def test_single_dot_namespace(self):
        ref = ToolRef.parse("client-a.Bash")
        assert ref.namespace == "client-a"
        assert ref.name == "Bash"

    def test_multi_level_namespace(self):
        ref = ToolRef.parse("org.team.Bash")
        assert ref.namespace == "org.team"
        assert ref.name == "Bash"

    def test_str_default_namespace_omits_prefix(self):
        ref = ToolRef(namespace=DEFAULT_NAMESPACE, name="Bash")
        assert str(ref) == "Bash"

    def test_str_non_default_namespace_includes_prefix(self):
        ref = ToolRef(namespace="client-a", name="Bash")
        assert str(ref) == "client-a.Bash"

    def test_round_trip_bare(self):
        assert str(ToolRef.parse("Read")) == "Read"

    def test_round_trip_namespaced(self):
        assert str(ToolRef.parse("ns.Tool")) == "ns.Tool"

    def test_empty_ref_raises(self):
        with pytest.raises(ValueError, match="cannot be empty"):
            ToolRef.parse("")

    def test_dot_only_raises(self):
        with pytest.raises(ValueError, match="Invalid tool reference"):
            ToolRef.parse(".")

    def test_leading_dot_raises(self):
        with pytest.raises(ValueError, match="Invalid tool reference"):
            ToolRef.parse(".Bash")

    def test_trailing_dot_raises(self):
        with pytest.raises(ValueError, match="Invalid tool reference"):
            ToolRef.parse("Bash.")


# ============================================================
# 命名空间注册 + 碰撞
# ============================================================


def _make_dummy_fn():
    async def dummy(x: str) -> str:
        """A dummy tool."""
        return x

    return dummy


class TestNamespaceRegistration:
    """ToolRegistry 命名空间注册行为测试。"""

    def test_register_default_namespace(self):
        reg = ToolRegistry()
        reg.register(name="MyTool")(_make_dummy_fn())
        tool = reg.get_tool("MyTool")
        assert tool is not None
        assert tool.namespace == DEFAULT_NAMESPACE

    def test_register_explicit_namespace(self):
        reg = ToolRegistry()
        reg.register(name="MyTool", namespace="client-a")(_make_dummy_fn())
        tool = reg.get_tool("MyTool", namespace="client-a")
        assert tool is not None
        assert tool.namespace == "client-a"

    def test_same_namespace_collision_raises(self):
        reg = ToolRegistry()
        reg.register(name="Bash")(_make_dummy_fn())
        with pytest.raises(ValueError, match="already registered"):
            reg.register(name="Bash")(_make_dummy_fn())

    def test_different_namespace_same_name_coexist(self):
        reg = ToolRegistry()
        reg.register(name="Bash", namespace="default")(_make_dummy_fn())
        reg.register(name="Bash", namespace="client-a")(_make_dummy_fn())
        assert reg.get_tool("Bash", "default") is not None
        assert reg.get_tool("Bash", "client-a") is not None
        assert len(reg.tools) == 2

    def test_tools_property_flattens_namespaces(self):
        reg = ToolRegistry()
        reg.register(name="A", namespace="ns1")(_make_dummy_fn())
        reg.register(name="B", namespace="ns2")(_make_dummy_fn())
        reg.register(name="C")(_make_dummy_fn())
        assert len(reg.tools) == 3


# ============================================================
# resolve()
# ============================================================


class TestResolve:
    """ToolRegistry.resolve() 测试。"""

    def test_resolve_bare_name(self):
        reg = ToolRegistry()
        reg.register(name="Bash")(_make_dummy_fn())
        tool = reg.resolve("Bash")
        assert tool is not None
        assert tool.name == "Bash"
        assert tool.namespace == DEFAULT_NAMESPACE

    def test_resolve_namespaced(self):
        reg = ToolRegistry()
        reg.register(name="Bash", namespace="client-a")(_make_dummy_fn())
        tool = reg.resolve("client-a.Bash")
        assert tool is not None
        assert tool.namespace == "client-a"

    def test_resolve_nonexistent_returns_none(self):
        reg = ToolRegistry()
        assert reg.resolve("NonExist") is None

    def test_resolve_wrong_namespace_returns_none(self):
        reg = ToolRegistry()
        reg.register(name="Bash")(_make_dummy_fn())
        assert reg.resolve("other.Bash") is None

    def test_resolve_malformed_ref_returns_none(self):
        reg = ToolRegistry()
        reg.register(name="Bash")(_make_dummy_fn())
        assert reg.resolve("") is None
        assert reg.resolve(".") is None
        assert reg.resolve(".Bash") is None
        assert reg.resolve("Bash.") is None


# ============================================================
# llm_name / effective_llm_name
# ============================================================


class TestLlmName:
    """Tool llm_name 和 effective_llm_name 测试。"""

    def test_default_llm_name_is_none(self):
        reg = ToolRegistry()
        reg.register(name="Bash")(_make_dummy_fn())
        tool = reg.get_tool("Bash")
        assert tool is not None
        assert tool.llm_name is None
        assert tool.effective_llm_name == "Bash"

    def test_explicit_llm_name(self):
        reg = ToolRegistry()
        reg.register(name="Bash", llm_name="Shell")(_make_dummy_fn())
        tool = reg.get_tool("Bash")
        assert tool is not None
        assert tool.llm_name == "Shell"
        assert tool.effective_llm_name == "Shell"

    def test_to_openai_uses_effective_llm_name(self):
        reg = ToolRegistry()
        reg.register(name="Bash", llm_name="Shell")(_make_dummy_fn())
        tool = reg.get_tool("Bash")
        assert tool is not None
        schema = tool.to_openai()
        assert schema["function"]["name"] == "Shell"

    def test_to_openai_default_uses_name(self):
        reg = ToolRegistry()
        reg.register(name="Bash")(_make_dummy_fn())
        tool = reg.get_tool("Bash")
        assert tool is not None
        schema = tool.to_openai()
        assert schema["function"]["name"] == "Bash"


class TestToolValidation:
    """Tool 字段校验测试。"""

    def test_empty_namespace_raises(self):
        from wing.schema import Tool

        with pytest.raises(ValueError, match="namespace must not be empty"):
            Tool(
                name="Bash",
                namespace="",
                description="",
                params=[],
                function=lambda: None,
            )

    def test_empty_llm_name_raises(self):
        from wing.schema import Tool

        with pytest.raises(ValueError, match="llm_name must not be empty"):
            Tool(
                name="Bash",
                llm_name="",
                description="",
                params=[],
                function=lambda: None,
            )

    def test_none_llm_name_ok(self):
        from wing.schema import Tool

        tool = Tool(
            name="Bash", llm_name=None, description="", params=[], function=lambda: None
        )
        assert tool.effective_llm_name == "Bash"
