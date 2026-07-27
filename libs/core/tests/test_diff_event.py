"""Tests that DiffContentEvent carries the executing tool call id.

Concurrent tool execution makes events arrive out of order at frontends;
the tool_call_id lets the TUI anchor each diff under its own ToolCall cell
instead of appending in completion order. Mirrors test_feedback.py's use of
the per-task _current_tool_call_id contextvar.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.agent import _current_tool_call_id
from wing.event import DiffContentEvent
from wing.tools.experimental import better_edit
from wing.tools.file import edit_file, write_file


class _StubAgent:
    """Minimal agent stub capturing emitted events."""

    def __init__(self) -> None:
        self.session_id = "sess-test"
        self.state: dict = {}
        self.events: list = []

    def emit(self, event) -> None:
        self.events.append(event)


def _diffs(agent: _StubAgent) -> list[DiffContentEvent]:
    return [e for e in agent.events if isinstance(e, DiffContentEvent)]


@pytest.mark.asyncio
async def test_write_emits_diff_with_current_tool_call_id(tmp_path: Path):
    agent = _StubAgent()
    token = _current_tool_call_id.set("call_write_1")
    try:
        await write_file(str(tmp_path / "new.txt"), "hello", agent=agent)  # type: ignore[arg-type]
    finally:
        _current_tool_call_id.reset(token)

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == "call_write_1"
    assert diff.old_text is None  # new file
    assert diff.new_text == "hello"


@pytest.mark.asyncio
async def test_edit_emits_diff_with_current_tool_call_id(tmp_path: Path):
    p = tmp_path / "f.txt"
    p.write_text("foo bar")
    agent = _StubAgent()
    token = _current_tool_call_id.set("call_edit_1")
    try:
        await edit_file(str(p), "foo", "baz", agent=agent)  # type: ignore[arg-type]
    finally:
        _current_tool_call_id.reset(token)

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == "call_edit_1"
    assert diff.old_text == "foo bar"
    assert diff.new_text == "baz bar"


@pytest.mark.asyncio
async def test_better_edit_emits_diff_with_current_tool_call_id(tmp_path: Path):
    p = tmp_path / "f.txt"
    p.write_text("alpha\nbeta\n")
    agent = _StubAgent()
    token = _current_tool_call_id.set("call_better_1")
    try:
        await better_edit(str(p), "alpha", "gamma", agent=agent)  # type: ignore[arg-type]
    finally:
        _current_tool_call_id.reset(token)

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == "call_better_1"
    assert diff.new_text == "gamma\nbeta\n"


@pytest.mark.asyncio
async def test_diff_tool_call_id_empty_outside_tool_context(tmp_path: Path):
    # No contextvar set (e.g., direct invocation) → empty string, which the
    # frontend treats as "anchor unknown, append".
    agent = _StubAgent()
    await write_file(str(tmp_path / "x.txt"), "x", agent=agent)  # type: ignore[arg-type]

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == ""
