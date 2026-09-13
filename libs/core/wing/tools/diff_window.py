# wing/tools/diff_window.py
"""Diff payload windowing — the shape contract of ``DiffContentEvent`` payloads.

A ``DiffContentEvent`` used to carry the **whole file** on both sides. Every
Edit therefore shipped hundreds of kilobytes over the wire and made the
frontend feed tens of thousands of lines into the syntect state machine on
resume (99.7% of the first-frame layout cost in the 2026-09-13 measurement).

The payload is windowed instead: ``old_text`` / ``new_text`` carry only the
**changed region plus 3 context lines** on each side, and the two new fields
``old_start_line`` / ``new_start_line`` tell the frontend which absolute file
line each window starts at. The frontend renders exactly what it is given —
there is no second collapsing policy on that side.

Contract (see ``openspec/changes/diff-payload-window``):

  - windows are line-aligned: the changed region's full lines, ± 3 context
    lines, clipped at the file boundaries (never padded),
  - ``replace_all`` produces one event **per match position** (same
    ``tool_call_id``), in left-to-right order,
  - ``Write`` / new files keep the full payload (``old_text=None`` => all add).

The producers here are the only place that knows the match positions; the
Rust side only re-diffs the two window texts with the same "lines" semantics.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass

from wing.event import DiffContentEvent

# Context lines kept before and after the changed region on each side.
DIFF_CONTEXT_LINES = 3


def split_lines(text: str) -> list[str]:
    """Split ``text`` the way Rust's ``str::lines()`` does.

    Rust's ``lines()`` splits on ``\\n``, drops the final empty element for a
    trailing newline and strips a trailing ``\\r`` from every line; Python's
    ``str.splitlines()`` also splits on ``\\v``/``\\f``/``\\x1c``/``\\u2028``,
    which would desynchronize the two sides' line counts. The window text is
    re-joined with ``\\n``, so a CRLF file is normalized to LF inside a diff
    window (display only — the file on disk is untouched).
    """
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [line[:-1] if line.endswith("\r") else line for line in lines]


def find_all(content: str, needle: str) -> list[int]:
    """All non-overlapping match positions of ``needle`` in ``content``.

    Mirrors ``str.replace`` / ``str.count`` semantics (left to right, no
    overlap: ``"aaa".find_all("aa") == [0]``), so the positions line up with
    the string that ``content.replace`` would produce.
    """
    if not needle:
        return []
    positions: list[int] = []
    start = 0
    while True:
        pos = content.find(needle, start)
        if pos == -1:
            return positions
        positions.append(pos)
        start = pos + len(needle)


@dataclass(frozen=True)
class DiffWindow:
    """One line-aligned window: texts plus their absolute start lines."""

    old_text: str
    new_text: str
    old_start_line: int
    new_start_line: int


def _span_lines(text: str, start: int, end: int) -> tuple[int, int]:
    """1-based inclusive line span covered by ``text[start:end]``.

    The span is expanded to whole lines (a match starting mid-line belongs to
    that line). A match ending with a newline ends on the line that newline
    terminates — not on the (empty) next line. An empty span (pure deletion /
    insertion point) resolves to the line containing ``start``.
    """
    first = text.count("\n", 0, start) + 1
    last = text.count("\n", 0, end - 1) + 1 if end > start else first
    return first, last


def _clip(
    first: int, last: int, total: int, context: int = DIFF_CONTEXT_LINES
) -> tuple[int, int]:
    """Expand ``[first, last]`` by ``context`` lines and clip to the file.

    Returns an inclusive 1-based range; an empty file yields ``(1, 0)`` so the
    slice is empty (no padding, no placeholder lines).
    """
    return max(1, first - context), min(total, last + context)


def window_for_replacement(
    old_content: str,
    new_content: str,
    old_start: int,
    old_len: int,
    new_start: int,
    new_len: int,
    context: int = DIFF_CONTEXT_LINES,
) -> DiffWindow:
    """Window around a replacement of ``old_content[old_start:old_start+old_len]``
    with ``new_content[new_start:new_start+new_len]``.

    Context lines are taken from each revision independently, so a change that
    adds 20 lines shows 3 trailing context lines on the new side but only its
    3 old neighbours on the old side — the same shape a git hunk has.
    """
    old_lines = split_lines(old_content)
    new_lines = split_lines(new_content)

    old_first, old_last = _span_lines(old_content, old_start, old_start + old_len)
    new_first, new_last = _span_lines(new_content, new_start, new_start + new_len)

    old_lo, old_hi = _clip(old_first, old_last, len(old_lines), context)
    new_lo, new_hi = _clip(new_first, new_last, len(new_lines), context)

    return DiffWindow(
        old_text="\n".join(old_lines[old_lo - 1 : old_hi]),
        new_text="\n".join(new_lines[new_lo - 1 : new_hi]),
        old_start_line=old_lo,
        new_start_line=new_lo,
    )


def windows_for_replacements(
    old_content: str,
    new_content: str,
    old_len: int,
    new_len: int,
    positions: Sequence[int],
    context: int = DIFF_CONTEXT_LINES,
) -> list[DiffWindow]:
    """One window per match position of the same replacement.

    ``positions`` are match starts in ``old_content`` (left to right). Each
    earlier replacement shifts every later match in the new content by the
    constant ``new_len - old_len``, which is how the new-side position (and
    therefore ``new_start_line``) is derived.
    """
    delta = new_len - old_len
    return [
        window_for_replacement(
            old_content,
            new_content,
            old_start=pos,
            old_len=old_len,
            new_start=pos + i * delta,
            new_len=new_len,
            context=context,
        )
        for i, pos in enumerate(positions)
    ]


def build_diff_events(
    *,
    session_id: str,
    path: str,
    tool_call_id: str,
    old_content: str,
    new_content: str,
    old_len: int,
    new_len: int,
    positions: Sequence[int],
) -> list[DiffContentEvent]:
    """Windowed ``DiffContentEvent`` list for a (possibly repeated) replacement.

    One event per match position, same ``tool_call_id``, left-to-right order —
    the frontend anchors each to the same ToolCall cell, appending after the
    previous diff sibling, so live and replay produce the same cell order.
    """
    return [
        DiffContentEvent(
            session_id=session_id,
            path=path,
            old_text=window.old_text,
            new_text=window.new_text,
            old_start_line=window.old_start_line,
            new_start_line=window.new_start_line,
            tool_call_id=tool_call_id,
        )
        for window in windows_for_replacements(
            old_content, new_content, old_len, new_len, positions
        )
    ]
