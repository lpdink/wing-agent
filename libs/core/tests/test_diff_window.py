"""Windowing of DiffContentEvent payloads (`wing.tools.diff_window`).

The payload used to be the whole file on both sides; it is now the changed
region ± 3 context lines plus absolute start lines, and `replace_all` emits
one event per match position. These tests lock the boundary rules (file
start/end, mid-line matches, short files), the absolute line arithmetic
under repeated replacements, and the payload-size contract.
"""

from __future__ import annotations

import json

import pytest

from wing.event import DiffContentEvent, serialize_event
from wing.tools.diff_window import (
    DIFF_CONTEXT_LINES,
    build_diff_events,
    find_all,
    split_lines,
    window_for_replacement,
    windows_for_replacements,
)


def numbered_file(total: int) -> str:
    """A file of `total` lines, "line N" each (no trailing newline)."""
    return "\n".join(f"line {i}" for i in range(1, total + 1))


# ── line splitting / matching semantics ────────────────────────────────


def test_split_lines_matches_rust_lines_semantics():
    assert split_lines("a\nb") == ["a", "b"]
    # Trailing newline does not add a phantom empty line.
    assert split_lines("a\nb\n") == ["a", "b"]
    # …but an intentional blank last line does.
    assert split_lines("a\n\n") == ["a", ""]
    # CRLF: the carriage return belongs to the terminator, not the text.
    assert split_lines("a\r\nb\r\n") == ["a", "b"]
    # Rust's lines() does NOT split on \v / \f / \u2028 (Python's splitlines would).
    assert split_lines("a\x0bb") == ["a\x0bb"]
    assert split_lines("") == []


def test_find_all_matches_str_replace_semantics():
    # Non-overlapping, left to right — exactly what str.replace consumes.
    assert find_all("aaa", "aa") == [0]
    assert "aaa".replace("aa", "b") == "ba"
    assert find_all("x.y.x", "x") == [0, 4]
    assert find_all("abc", "zz") == []
    assert find_all("abc", "") == []


# ── window shape ───────────────────────────────────────────────────────


def test_single_replacement_window_is_change_plus_context():
    old = numbered_file(20)
    new = old.replace("line 10", "LINE TEN")

    positions = find_all(old, "line 10")
    (w,) = windows_for_replacements(
        old, new, len("line 10"), len("LINE TEN"), positions
    )

    # Lines 7..13 = change line ± 3, nothing else.
    assert w.old_text == "\n".join(f"line {i}" for i in range(7, 14))
    assert w.new_text == "\n".join(
        ["line 7", "line 8", "line 9", "LINE TEN", "line 11", "line 12", "line 13"]
    )
    assert (w.old_start_line, w.new_start_line) == (7, 7)
    assert len(split_lines(w.old_text)) == 2 * DIFF_CONTEXT_LINES + 1


def test_window_clipped_at_file_start():
    old = numbered_file(10)
    new = old.replace("line 1", "LINE ONE")

    w = window_for_replacement(old, new, 0, len("line 1"), 0, len("LINE ONE"))

    assert w.old_start_line == 1
    assert w.new_start_line == 1
    assert split_lines(w.old_text)[0] == "line 1"  # no padding before line 1
    assert split_lines(w.old_text)[-1] == "line 4"


def test_window_clipped_at_file_end():
    old = numbered_file(10)
    new = old.replace("line 10", "LINE TEN")
    pos = old.rfind("line 10")

    w = window_for_replacement(old, new, pos, len("line 10"), pos, len("LINE TEN"))

    assert w.old_start_line == 7
    assert split_lines(w.old_text)[-1] == "line 10"  # nothing after the last line
    assert split_lines(w.new_text)[-1] == "LINE TEN"


def test_window_of_short_file_is_the_whole_file():
    old = "a\nb\nc"
    new = "a\nB\nc"

    w = window_for_replacement(old, new, 2, 1, 2, 1)

    assert (w.old_text, w.new_text) == ("a\nb\nc", "a\nB\nc")
    assert (w.old_start_line, w.new_start_line) == (1, 1)


def test_multi_line_change_keeps_per_side_context():
    old = numbered_file(30)
    # 3 old lines → 20 new lines.
    old_string = "\n".join(f"line {i}" for i in range(10, 13))
    new_string = "\n".join(f"NEW {i}" for i in range(20))
    new = old.replace(old_string, new_string)

    w = window_for_replacement(
        old,
        new,
        old.find(old_string),
        len(old_string),
        old.find(old_string),
        len(new_string),
    )

    assert split_lines(w.old_text) == [f"line {i}" for i in range(7, 16)]
    assert split_lines(w.new_text)[:3] == ["line 7", "line 8", "line 9"]
    assert split_lines(w.new_text)[3:23] == [f"NEW {i}" for i in range(20)]
    assert split_lines(w.new_text)[23:] == ["line 13", "line 14", "line 15"]
    assert (w.old_start_line, w.new_start_line) == (7, 7)


def test_mid_line_match_expands_to_full_line():
    old = "alpha\nbeta\ngamma"
    new = "alpha\nBETA! x\ngamma"

    w = window_for_replacement(old, new, 6, 4, 6, 8)

    # The whole "beta" line is the change line (not just its middle 4 chars).
    assert split_lines(w.old_text) == ["alpha", "beta", "gamma"]
    assert split_lines(w.new_text) == ["alpha", "BETA! x", "gamma"]
    assert w.old_start_line == 1


def test_replacement_at_file_start_deleting_first_line():
    old = "drop\nkeep\ntail"
    new = "keep\ntail"

    w = window_for_replacement(old, new, 0, len("drop\n"), 0, 0)

    assert w.old_text == "drop\nkeep\ntail"
    assert w.new_text == "keep\ntail"
    assert (w.old_start_line, w.new_start_line) == (1, 1)


def test_pure_deletion_window_points_at_the_deleted_line():
    old = "a\nb\nc\nd\ne"
    new = "a\nc\nd\ne"

    w = window_for_replacement(old, new, 2, len("b\n"), 2, 0)

    # old side: the deleted line is the change line; new side: the join point.
    assert (w.old_start_line, w.new_start_line) == (1, 1)
    assert "b" in split_lines(w.old_text)
    assert "b" not in split_lines(w.new_text)


# ── replace_all: one window per match ─────────────────────────────────


def test_replace_all_windows_track_accumulated_line_offsets():
    old = numbered_file(40)
    # Two matches: line 5 and line 15, each replaced by text that adds one line.
    lines = old.split("\n")
    lines[4] = "foo"  # line 5
    lines[14] = "foo"  # line 15
    old = "\n".join(lines)

    new_string = "bar\nbaz"  # +1 line per replacement
    new = old.replace("foo", new_string)
    positions = find_all(old, "foo")
    assert len(positions) == 2

    windows = windows_for_replacements(old, new, len("foo"), len(new_string), positions)
    assert len(windows) == 2

    first, second = windows
    # First match sits at old line 5 / new line 5 (nothing before it shifted).
    assert (first.old_start_line, first.new_start_line) == (2, 2)
    # Second match moved down by one line on the new side only.
    assert second.old_start_line == 12
    assert second.new_start_line == 13
    # Each window contains exactly one occurrence of the replaced text.
    assert first.old_text.count("foo") == 1
    assert second.old_text.count("foo") == 1
    assert set(split_lines(first.new_text)) >= {"bar", "baz"}


def test_replace_all_events_share_tool_call_id_and_keep_order():
    old = numbered_file(40)
    old = "\n".join("foo" if i in (4, 14, 24) else f"line {i + 1}" for i in range(40))
    new = old.replace("foo", "FOO")
    positions = find_all(old, "foo")

    events = build_diff_events(
        session_id="s1",
        path="/tmp/f.txt",
        tool_call_id="call_1",
        old_content=old,
        new_content=new,
        old_len=len("foo"),
        new_len=len("FOO"),
        positions=positions,
    )

    assert len(events) == 3
    assert all(isinstance(e, DiffContentEvent) for e in events)
    assert {e.tool_call_id for e in events} == {"call_1"}
    assert [e.old_start_line for e in events] == sorted(
        e.old_start_line for e in events
    )
    assert all(e.old_start_line == e.new_start_line for e in events)


def test_build_diff_events_single_position():
    old = numbered_file(10)
    new = old.replace("line 3", "LINE THREE")

    events = build_diff_events(
        session_id="s1",
        path="p",
        tool_call_id="c",
        old_content=old,
        new_content=new,
        old_len=len("line 3"),
        new_len=len("LINE THREE"),
        positions=find_all(old, "line 3"),
    )

    assert len(events) == 1
    assert events[0].new_start_line == 1
    assert "LINE THREE" in events[0].new_text


# ── payload size contract ─────────────────────────────────────────────


def test_payload_is_independent_of_file_size():
    def event_for(total: int) -> DiffContentEvent:
        old = numbered_file(total)
        new = old.replace("line 50", "LINE FIFTY")
        return build_diff_events(
            session_id="s",
            path="p",
            tool_call_id="c",
            old_content=old,
            new_content=new,
            old_len=len("line 50"),
            new_len=len("LINE FIFTY"),
            positions=find_all(old, "line 50"),
        )[0]

    small = event_for(100)
    large = event_for(10_000)

    assert small.old_text == large.old_text
    assert small.new_text == large.new_text
    assert (small.old_start_line, small.new_start_line) == (47, 47)
    # The event itself stays tiny: it carries a window, not a file.
    assert len(json.dumps(serialize_event(large)).encode()) < 2_000


def test_single_event_byte_bound():
    """Windowed events stay far below the 8 KB per-event bound for typical
    source files (the whole point of the change: the sync payload is no
    longer dominated by a handful of diff events)."""
    old = numbered_file(5_000)
    new = old.replace("line 2500", "LINE 2500 (edited)")

    (event,) = build_diff_events(
        session_id="s",
        path="src/main.rs",
        tool_call_id="call_x",
        old_content=old,
        new_content=new,
        old_len=len("line 2500"),
        new_len=len("LINE 2500 (edited)"),
        positions=find_all(old, "line 2500"),
    )

    encoded = json.dumps(serialize_event(event)).encode()
    assert len(encoded) < 8 * 1024, f"windowed event is {len(encoded)} bytes"


def test_window_does_not_grow_with_distant_occurrences():
    """A `replace_all` far apart must not merge into one file-sized window:
    each event carries only its own neighbourhood."""
    old_lines = [f"line {i}" for i in range(1, 2001)]
    old_lines[0] = "foo"
    old_lines[-1] = "foo"
    old = "\n".join(old_lines)
    new = old.replace("foo", "FOO")

    events = build_diff_events(
        session_id="s",
        path="p",
        tool_call_id="c",
        old_content=old,
        new_content=new,
        old_len=3,
        new_len=3,
        positions=find_all(old, "foo"),
    )

    assert len(events) == 2
    for event in events:
        assert event.old_text is not None
        assert len(event.old_text.encode()) < 200
        assert len(event.new_text.encode()) < 200
    assert {e.old_start_line for e in events} == {1, 1997}


def test_empty_new_string_window_has_no_phantom_lines():
    old = "a\nb\nc"
    new = "a\nc"

    (w,) = windows_for_replacements(old, new, 2, 0, [2])

    assert w.new_text == "a\nc"
    assert w.new_start_line == 1
    assert "\n\n" not in w.new_text


@pytest.mark.parametrize("match", ["a", "\nb", "a\nb\nc"])
def test_window_text_rebuilds_the_source_lines(match: str):
    old = "a\nb\nc"
    pos = old.find(match)
    new = old[:pos] + match.upper() + old[pos + len(match) :]

    (w,) = windows_for_replacements(old, new, len(match), len(match), [pos])

    # Every window line is a complete source line (re-joining reproduces them).
    assert all(line in old for line in split_lines(w.old_text))
    assert "\n".join(split_lines(w.old_text)) == w.old_text
    assert "\n".join(split_lines(w.new_text)) == w.new_text
