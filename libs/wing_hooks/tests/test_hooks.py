"""wing_hooks 三个官方 hook 的单元测试。

TDD：先定义行为，再实现。

三个 hook:
1. prefix_timestamp: before_user_message — 给用户消息前缀时间戳
2. truncate_tool_result: after_tool_call — 截断过长的 tool result
3. audit_edit_write: after_tool_call — 审计 Edit/Write 代码变更
"""

import re

import pytest

from wing.hook_registry import HookRegistry


# ============================================================
# prefix_timestamp hook
# ============================================================


class TestPrefixTimestamp:
    def test_prefix_timestamp(self):
        """before_user_message hook 在 content 之前加上当前时间戳"""
        from wing_hooks.prefix_timestamp import register_prefix_timestamp

        registry = HookRegistry()
        register_prefix_timestamp(registry)

        handlers = registry.handlers("before_user_message")
        assert len(handlers) == 1

        result = registry.invoke("before_user_message", "hello")
        assert result.startswith("[202")
        assert "hello" in result

    @pytest.mark.asyncio
    async def test_prefix_timestamp_async(self):
        """invoke_async 也支持"""
        from wing_hooks.prefix_timestamp import register_prefix_timestamp

        registry = HookRegistry()
        register_prefix_timestamp(registry)

        result = await registry.invoke_async("before_user_message", "hello")
        assert result.startswith("[202")
        assert "hello" in result

    def test_timestamp_format(self):
        """时间戳格式为 [YYYY-MM-DD HH:MM:SS]"""
        from wing_hooks.prefix_timestamp import register_prefix_timestamp

        registry = HookRegistry()
        register_prefix_timestamp(registry)

        result = registry.invoke("before_user_message", "test msg")
        # 验证格式：[YYYY-MM-DD HH:MM:SS] test msg
        pattern = r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\]"
        assert re.match(pattern, result)

    def test_none_content_preserved(self):
        """None content 不添加时间戳，handler 返回 None"""
        from wing_hooks.prefix_timestamp import register_prefix_timestamp

        registry = HookRegistry()
        register_prefix_timestamp(registry)

        result = registry.invoke("before_user_message", None)
        assert result is None

    def test_empty_content_not_modified(self):
        """空字符串 content 不添加时间戳"""
        from wing_hooks.prefix_timestamp import register_prefix_timestamp

        registry = HookRegistry()
        register_prefix_timestamp(registry)

        result = registry.invoke("before_user_message", "")
        assert result == ""


# ============================================================
# truncate_tool_result hook
# ============================================================


class TestTruncateToolResult:
    def test_short_result_not_truncated(self):
        """Results shorter than 50000 chars are not truncated."""
        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        short_result = "short output"
        result = registry.invoke("after_tool_call", short_result, tool_name="Bash")
        assert result == short_result

    def test_exact_threshold_not_truncated(self):
        """Results of exactly 50000 chars are not truncated."""
        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        exact_result = "x" * 50000
        result = registry.invoke("after_tool_call", exact_result, tool_name="Bash")
        assert result == exact_result

    def test_long_result_truncated(self):
        """Results over 50000 chars are truncated: keep first/last 100 chars."""
        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        long_result = "A" * 49000 + "B" * 2000  # 51000 > 50000
        result = registry.invoke("after_tool_call", long_result, tool_name="Bash")

        assert len(result) < len(long_result)
        assert result.startswith("A" * 100)
        assert result.endswith("B" * 100)
        assert "truncated" in result

    def test_truncation_marker_contains_info_and_file_path(self):
        """Truncation marker includes original length and temp file path."""
        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        long_result = "x" * 55000
        result = registry.invoke("after_tool_call", long_result, tool_name="Bash")
        assert "55000" in result
        assert "truncated" in result
        assert "full result saved to" in result
        assert "Read tool" in result

    def test_full_result_saved_to_temp_file(self):
        """Full result is saved to a persistent temp file."""
        import re

        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        long_result = "Hello" + "x" * 54995  # 55000 chars
        result = registry.invoke("after_tool_call", long_result, tool_name="Bash")

        match = re.search(r"full result saved to: (.+?\.txt)", result)
        assert match is not None
        file_path = match.group(1)

        from pathlib import Path

        saved_content = Path(file_path).read_text(encoding="utf-8")
        assert saved_content == long_result

        Path(file_path).unlink()

    def test_none_result_not_truncated(self):
        """None result 不截断"""
        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        result = registry.invoke("after_tool_call", None, tool_name="Bash")
        assert result is None

    def test_truncate_all_tools(self):
        """截断对所有 tool name 都生效（不限于 Bash）"""
        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        long_result = "x" * 55000
        result = registry.invoke("after_tool_call", long_result, tool_name="Edit")
        assert len(result) < len(long_result)
        assert "truncated" in result

    @pytest.mark.asyncio
    async def test_truncate_async(self):
        """invoke_async supports truncation."""
        from wing_hooks.truncate_tool_result import register_truncate_tool_result

        registry = HookRegistry()
        register_truncate_tool_result(registry)

        long_result = "x" * 55000
        result = await registry.invoke_async(
            "after_tool_call", long_result, tool_name="Bash"
        )
        assert len(result) < len(long_result)


# ============================================================
# audit_edit_write hook — before_tool_call
# ============================================================


class TestAuditEditWrite:
    def test_edit_tc_not_modified(self):
        """Edit 工具调用时 handler 不修改 ToolCall（返回 None → 保留原始 tc）"""
        from wing.schema import ToolCall
        from wing_hooks.audit_edit_write import register_audit_edit_write

        registry = HookRegistry()
        register_audit_edit_write(registry)

        tc = ToolCall(
            id="1",
            name="Edit",
            arguments={"path": "/tmp/a.py", "old_string": "x", "new_string": "y"},
        )
        result = registry.invoke("before_tool_call", tc, tool_name=tc.name)
        # handler 返回 None → 保留原始 tc 不修改
        assert result == tc

    def test_write_tc_not_modified(self):
        """Write 工具调用时 handler 不修改 ToolCall"""
        from wing.schema import ToolCall
        from wing_hooks.audit_edit_write import register_audit_edit_write

        registry = HookRegistry()
        register_audit_edit_write(registry)

        tc = ToolCall(
            id="2", name="Write", arguments={"path": "/tmp/b.py", "content": "hello"}
        )
        result = registry.invoke("before_tool_call", tc, tool_name=tc.name)
        assert result == tc

    def test_other_tools_not_affected(self):
        """其他工具调用不触发审计上报，handler 返回 None → 保留原始 tc"""
        from wing.schema import ToolCall
        from wing_hooks.audit_edit_write import register_audit_edit_write

        registry = HookRegistry()
        register_audit_edit_write(registry)

        for tool_name in ["Bash", "Grep", "Read", "WebFetch"]:
            tc = ToolCall(id="3", name=tool_name, arguments={"command": "ls"})
            result = registry.invoke("before_tool_call", tc, tool_name=tool_name)
            assert result == tc

    def test_none_tc_preserved(self):
        """None ToolCall 保留原始 None"""
        from wing_hooks.audit_edit_write import register_audit_edit_write

        registry = HookRegistry()
        register_audit_edit_write(registry)

        result = registry.invoke("before_tool_call", None)
        assert result is None
