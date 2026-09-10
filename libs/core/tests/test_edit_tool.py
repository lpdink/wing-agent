"""Edit tool tests — Claude Code-aligned parameter names.

The Edit tool speaks the Claude Code dialect: `old_string` / `new_string`.
Models fine-tuned on Claude Code emit these names as instinct, so the
LLM-facing schema (to_openai) is a contract worth locking. A drifted name
surfaces at exec time as "missing 1 required positional argument: ..."
because ToolExecutor filters model args by the tool's signature.
"""

from __future__ import annotations

import inspect
from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.file import edit_file


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
        # Full-file diff event still emitted for the frontend.
        assert len(ctx.events) == 1
        assert ctx.events[0].old_text == "foo bar\n"
        assert ctx.events[0].new_text == "foo baz\n"

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
