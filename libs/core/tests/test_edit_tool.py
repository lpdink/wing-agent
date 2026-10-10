"""Edit tool tests — Claude Code-aligned parameter names.

The Edit tool speaks the Claude Code dialect: `old_string` / `new_string`.
Models fine-tuned on Claude Code emit these names as instinct, so the
LLM-facing schema (to_openai) is a contract worth locking. A drifted name
surfaces at exec time as "missing 1 required positional argument: ..."
because ToolExecutor filters model args by the tool's signature.
"""

from __future__ import annotations

import inspect
import os
import stat
from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.builtin.edit import edit_file


class _StubCtx:
    """Minimal ToolContext stub capturing emitted events."""

    def __init__(self) -> None:
        self.session_id = "sess-edit-test"
        self.events: list = []

    def emit(self, event) -> None:
        self.events.append(event)


def _bind_like_executor(arguments: dict) -> dict:
    """Mirror ToolExecutor's arg filtering: keep only signature params.

    ToolExecutor drops model args that are not tool signature params, so a
    dialect mismatch turns into a missing-argument TypeError at call time.
    """
    params = inspect.signature(edit_file).parameters
    return {k: v for k, v in arguments.items() if k in params}


class TestClaudeParamNames:
    """LLM-facing schema exposes Claude Code names."""

    def test_openai_schema_uses_claude_param_names(self):
        tool = tool_registry.get_tool("Edit")
        assert tool is not None
        schema = tool.to_openai()
        params = schema["function"]["parameters"]

        assert list(params["properties"]) == [
            "path",
            "old_string",
            "new_string",
            "replace_all",
        ]
        assert params["required"] == ["path", "old_string", "new_string"]

    def test_legacy_dialect_absent_from_schema(self):
        tool = tool_registry.get_tool("Edit")
        assert tool is not None
        properties = tool.to_openai()["function"]["parameters"]["properties"]
        assert "old_block" not in properties
        assert "new_block" not in properties


class TestClaudeDialectBinding:
    """Claude Code dialect args bind and execute (regression)."""

    @pytest.mark.asyncio
    async def test_claude_args_bind_and_execute(self, tmp_path: Path):
        p = tmp_path / "f.py"
        p.write_text("foo bar\n")
        ctx = _StubCtx()

        # What a Claude Code-tuned model sends for a minimal edit.
        call_args = _bind_like_executor(
            {"path": str(p), "old_string": "bar", "new_string": "baz"}
        )
        result = await edit_file(**call_args, ctx=ctx)  # type: ignore[arg-type]

        assert "edit: ok @ line 1" in result
        assert p.read_text() == "foo baz\n"
        # Windowed diff event for the frontend: this file is shorter than the
        # window, so the window is the whole file (re-joined without the
        # trailing newline — the frontend diffs lines).
        assert len(ctx.events) == 1
        assert ctx.events[0].old_text == "foo bar"
        assert ctx.events[0].new_text == "foo baz"
        assert (ctx.events[0].old_start_line, ctx.events[0].new_start_line) == (1, 1)

    @pytest.mark.asyncio
    async def test_replace_all_kwarg(self, tmp_path: Path):
        p = tmp_path / "f.txt"
        p.write_text("foo foo foo")
        ctx = _StubCtx()

        call_args = _bind_like_executor(
            {
                "path": str(p),
                "old_string": "foo",
                "new_string": "x",
                "replace_all": True,
            }
        )
        result = await edit_file(**call_args, ctx=ctx)  # type: ignore[arg-type]

        assert "edit: ok (3 replacements)" in result
        assert p.read_text() == "x x x"


class TestEditErrors:
    """Error text names old_string so the model can self-correct."""

    @pytest.mark.asyncio
    async def test_not_found_mentions_old_string(self, tmp_path: Path):
        p = tmp_path / "f.txt"
        p.write_text("abc")
        with pytest.raises(ToolError, match="old_string not found"):
            await edit_file(str(p), "zzz", "x", ctx=_StubCtx())  # type: ignore[arg-type]

    @pytest.mark.asyncio
    async def test_ambiguous_hints_replace_all(self, tmp_path: Path):
        p = tmp_path / "f.txt"
        p.write_text("foo\nfoo\n")
        with pytest.raises(ToolError, match="use replace_all=True"):
            await edit_file(str(p), "foo", "x", ctx=_StubCtx())  # type: ignore[arg-type]

    @pytest.mark.asyncio
    async def test_empty_old_string_rejected(self, tmp_path: Path):
        p = tmp_path / "f.txt"
        p.write_text("abc")
        with pytest.raises(ToolError, match="old_string cannot be empty"):
            await edit_file(str(p), "", "x", ctx=_StubCtx())  # type: ignore[arg-type]


class TestEditDurability:
    """Edit 走共享原子写原语（tmp + fsync + replace）：覆盖保留权限位。"""

    @pytest.mark.asyncio
    async def test_edit_preserves_mode(self, tmp_path: Path):
        p = tmp_path / "script.sh"
        p.write_text("#!/bin/sh\necho old\n", encoding="utf-8")
        p.chmod(0o755)
        ctx = _StubCtx()

        await edit_file(str(p), "old", "new", ctx=ctx)  # type: ignore[arg-type]

        assert p.read_text() == "#!/bin/sh\necho new\n"
        assert stat.S_IMODE(p.stat().st_mode) == 0o755


class TestWriteTargetPolicy:
    """Edit 与 Write 共用同一份写入目标政策（`tools/internal/write_target.py`）。"""

    @pytest.mark.asyncio
    async def test_edit_writes_through_symlink(self, tmp_path: Path):
        """末段是符号链接：编辑真实目标，链接保留（与 Write 一致）。"""
        real = tmp_path / "real.txt"
        real.write_text("old value\n", encoding="utf-8")
        link = tmp_path / "link.txt"
        link.symlink_to(real)
        ctx = _StubCtx()

        await edit_file(str(link), "old value", "new value", ctx=ctx)  # type: ignore[arg-type]

        assert link.is_symlink()
        assert real.read_text(encoding="utf-8") == "new value\n"

    @pytest.mark.asyncio
    @pytest.mark.skipif(
        hasattr(os, "geteuid") and os.geteuid() == 0,
        reason="root bypasses the target's read-only bit",
    )
    async def test_edit_refuses_readonly_target(self, tmp_path: Path):
        """只读目标：拒绝编辑（与 Write 一致，不给"换个工具绕过"的口子）。"""
        target = tmp_path / "ro.txt"
        target.write_text("protected", encoding="utf-8")
        target.chmod(0o444)
        ctx = _StubCtx()

        with pytest.raises(ToolError, match="Permission denied"):
            await edit_file(str(target), "protected", "hacked", ctx=ctx)  # type: ignore[arg-type]

        assert target.read_text(encoding="utf-8") == "protected"
        assert stat.S_IMODE(target.stat().st_mode) == 0o444
        assert ctx.events == []  # 失败不 emit diff 事件
