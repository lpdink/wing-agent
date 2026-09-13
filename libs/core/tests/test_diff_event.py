"""DiffContentEvent payload tests — anchor id + windowed payload shape.

Two contracts live here:

1. `tool_call_id` associates a diff with the tool call that produced it.
   Concurrent tool execution makes events arrive out of order at frontends;
   the id lets the TUI anchor each diff under its own ToolCall cell instead
   of appending in completion order. Mirrors test_feedback.py's use of the
   per-task `_current_tool_call_id` contextvar.
2. The payload is a **window** (changed region ± context lines, absolute
   start lines) for Edit/BetterEdit, and the full content for Write. The
   window arithmetic itself is unit-tested in test_diff_window.py; these
   tests lock the wiring the tools actually emit.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.agent.tool_executor import _current_tool_call_id
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


def _numbered(total: int) -> str:
    return "\n".join(f"line {i}" for i in range(1, total + 1))


@pytest.mark.asyncio
async def test_write_emits_diff_with_current_tool_call_id(tmp_path: Path):
    agent = _StubAgent()
    token = _current_tool_call_id.set("call_write_1")
    try:
        await write_file(str(tmp_path / "new.txt"), "hello", ctx=agent)  # type: ignore[arg-type]
    finally:
        _current_tool_call_id.reset(token)

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == "call_write_1"
    assert diff.old_text is None  # new file
    assert diff.new_text == "hello"


@pytest.mark.asyncio
async def test_write_overwrite_keeps_full_payload(tmp_path: Path):
    # Write is NOT windowed: whole-old / whole-new, both starting at line 1.
    p = tmp_path / "f.txt"
    old = _numbered(50)
    p.write_text(old)
    agent = _StubAgent()
    new = _numbered(50).replace("line 50", "LINE FIFTY")

    await write_file(str(p), new, ctx=agent)  # type: ignore[arg-type]

    (diff,) = _diffs(agent)
    assert diff.old_text == old
    assert diff.new_text == new
    assert (diff.old_start_line, diff.new_start_line) == (1, 1)


@pytest.mark.asyncio
async def test_edit_emits_windy_payload_only(tmp_path: Path):
    p = tmp_path / "f.txt"
    p.write_text("foo bar")
    agent = _StubAgent()
    token = _current_tool_call_id.set("call_edit_1")
    try:
        await edit_file(str(p), "foo", "baz", ctx=agent)  # type: ignore[arg-type]
    finally:
        _current_tool_call_id.reset(token)

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == "call_edit_1"
    assert diff.old_text == "foo bar"
    assert diff.new_text == "baz bar"
    assert (diff.old_start_line, diff.new_start_line) == (1, 1)


@pytest.mark.asyncio
async def test_edit_windows_out_the_unchanged_rest_of_the_file(tmp_path: Path):
    p = tmp_path / "f.txt"
    p.write_text(_numbered(40))
    agent = _StubAgent()

    await edit_file(str(p), "line 20", "LINE TWENTY", ctx=agent)  # type: ignore[arg-type]

    (diff,) = _diffs(agent)
    assert (diff.old_start_line, diff.new_start_line) == (17, 17)
    assert diff.old_text == "\n".join(f"line {i}" for i in range(17, 24))
    assert diff.new_text == "\n".join(
        [
            *[f"line {i}" for i in range(17, 20)],
            "LINE TWENTY",
            *[f"line {i}" for i in range(21, 24)],
        ]
    )
    # The rest of the file is simply not in the payload.
    assert diff.old_text is not None
    assert diff.old_text.splitlines()[0] == "line 17"
    assert diff.old_text.splitlines()[-1] == "line 23"


@pytest.mark.asyncio
async def test_edit_replace_all_emits_one_event_per_match(tmp_path: Path):
    p = tmp_path / "f.txt"
    lines = [f"line {i}" for i in range(1, 31)]
    lines[4] = "foo"  # line 5
    lines[14] = "foo"  # line 15
    lines[24] = "foo"  # line 25
    p.write_text("\n".join(lines))
    agent = _StubAgent()
    token = _current_tool_call_id.set("call_all_1")
    try:
        await edit_file(str(p), "foo", "FOO", replace_all=True, ctx=agent)  # type: ignore[arg-type]
    finally:
        _current_tool_call_id.reset(token)

    diffs = _diffs(agent)
    assert len(diffs) == 3, "one windowed event per match position"
    assert {d.tool_call_id for d in diffs} == {"call_all_1"}
    # Left-to-right order, absolute line numbers track the shifted file.
    assert [d.old_start_line for d in diffs] == [2, 12, 22]
    assert [d.new_start_line for d in diffs] == [2, 12, 22]
    assert all(d.old_text is not None and d.old_text.count("foo") == 1 for d in diffs)
    assert all(d.new_text.count("FOO") == 1 for d in diffs)


@pytest.mark.asyncio
async def test_edit_replace_all_line_delta_shifts_new_start_line(tmp_path: Path):
    # Each replacement adds a line → later windows sit lower in the new revision.
    p = tmp_path / "f.txt"
    lines = [f"line {i}" for i in range(1, 31)]
    lines[4] = "foo"
    lines[14] = "foo"
    p.write_text("\n".join(lines))
    agent = _StubAgent()

    await edit_file(str(p), "foo", "bar\nbaz", replace_all=True, ctx=agent)  # type: ignore[arg-type]

    first, second = _diffs(agent)
    assert (first.old_start_line, first.new_start_line) == (2, 2)
    assert (second.old_start_line, second.new_start_line) == (12, 13)


@pytest.mark.asyncio
async def test_edit_single_match_still_emits_one_event(tmp_path: Path):
    p = tmp_path / "f.txt"
    p.write_text(_numbered(30))
    agent = _StubAgent()

    await edit_file(str(p), "line 4", "LINE FOUR", replace_all=True, ctx=agent)  # type: ignore[arg-type]

    assert len(_diffs(agent)) == 1


@pytest.mark.asyncio
async def test_better_edit_emits_diff_with_current_tool_call_id(tmp_path: Path):
    p = tmp_path / "f.txt"
    p.write_text("alpha\nbeta\n")
    agent = _StubAgent()
    token = _current_tool_call_id.set("call_better_1")
    try:
        await better_edit(str(p), "alpha", "gamma", ctx=agent)  # type: ignore[arg-type]
    finally:
        _current_tool_call_id.reset(token)

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == "call_better_1"
    assert diff.new_text == "gamma\nbeta"


@pytest.mark.asyncio
async def test_better_edit_anchored_window_uses_the_replaced_span(tmp_path: Path):
    # `[upto]` replaces a span that is not the literal old_string; the window
    # must be taken around the actual replaced span.
    p = tmp_path / "f.txt"
    body = "\n".join(f"line {i}" for i in range(1, 21))
    p.write_text(f"head\n{body}\ntail\n")
    agent = _StubAgent()

    await better_edit(
        str(p),
        "line 6\nline 7\n[upto]\nline 10\nline 11",
        "REPLACED",
        ctx=agent,  # type: ignore[arg-type]
    )

    (diff,) = _diffs(agent)
    # The replaced span is lines 7..12 of the file; the window is ± 3 lines.
    assert (diff.old_start_line, diff.new_start_line) == (4, 4)
    assert diff.old_text is not None
    assert diff.old_text.splitlines()[0] == "line 3"  # file line 4
    assert diff.old_text.splitlines()[-1] == "line 14"  # file line 15
    assert "line 6" in diff.old_text  # head anchor of the replaced span
    assert "line 11" in diff.old_text  # tail anchor of the replaced span
    assert diff.new_text.splitlines()[3] == "REPLACED"
    assert "line 6" not in diff.new_text  # replaced span left the new revision


@pytest.mark.asyncio
async def test_diff_tool_call_id_empty_outside_tool_context(tmp_path: Path):
    # No contextvar set (e.g., direct invocation) → empty string, which the
    # frontend treats as "anchor unknown, append".
    agent = _StubAgent()
    await write_file(str(tmp_path / "x.txt"), "x", ctx=agent)  # type: ignore[arg-type]

    (diff,) = _diffs(agent)
    assert diff.tool_call_id == ""
