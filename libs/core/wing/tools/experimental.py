# wing/tools/experimental.py
"""Experimental tools: BetterEdit with [upto] anchored edit support.

Inspired by antirez's ds4-agent edit tool design.
"""

import os

from wing.agent import ToolContext, current_tool_call_id
from wing.event import DiffContentEvent
from wing.schema import ToolError
from wing.tool_registry import tool_registry

# Marker for anchored edit
UPTO_MARKER = "[upto]"


class BetterEditError(ToolError):
    """Exception for BetterEdit tool errors."""

    pass


def _find_old_span(content: str, old_string: str) -> tuple[int, int]:
    """Find the span to replace in content.

    Returns:
        (start, end) byte positions in content.

    Raises:
        BetterEditError: If match not found or ambiguous.
    """
    if UPTO_MARKER not in old_string:
        # Simple exact match mode
        pos = content.find(old_string)
        if pos == -1:
            total_lines = len(content.splitlines())
            if total_lines == 0:
                raise BetterEditError("old_string not found (file is empty)")
            first_lines = "\n".join(content.splitlines()[:3])
            raise BetterEditError(
                f"old_string not found in {total_lines} lines\nfile starts with:\n{first_lines}"
            )
        return pos, pos + len(old_string)

    # Anchored edit mode: old_string contains [upto]
    parts = old_string.split(UPTO_MARKER, 1)
    if len(parts) != 2:
        raise BetterEditError(f"old_string contains more than one {UPTO_MARKER} marker")

    head, tail = parts
    tail = tail.lstrip("\n")  # Allow newline after [upto] for readability

    if not head:
        raise BetterEditError(f"old_string before {UPTO_MARKER} (head) cannot be empty")
    if not tail or not tail.strip():
        raise BetterEditError(
            f"old_string after {UPTO_MARKER} (tail) must contain unique anchor text"
        )

    # Find unique head
    head_pos = content.find(head)
    if head_pos == -1:
        raise BetterEditError(f"{UPTO_MARKER} head not found in file")

    head_second = content.find(head, head_pos + 1)
    if head_second != -1:
        head_line1 = content[:head_pos].count("\n") + 1
        head_line2 = content[:head_second].count("\n") + 1
        raise BetterEditError(
            f"{UPTO_MARKER} head not unique (matches at lines {head_line1}, {head_line2})"
        )

    # Find unique tail after head
    tail_start = head_pos + len(head)
    tail_pos = content.find(tail, tail_start)
    if tail_pos == -1:
        raise BetterEditError(f"{UPTO_MARKER} tail not found after head")

    tail_second = content.find(tail, tail_pos + 1)
    if tail_second != -1:
        tail_line1 = content[:tail_pos].count("\n") + 1
        tail_line2 = content[:tail_second].count("\n") + 1
        raise BetterEditError(
            f"{UPTO_MARKER} tail not unique (matches at lines {tail_line1}, {tail_line2})"
        )

    # Return span from head start to tail end
    return head_pos, tail_pos + len(tail)


# reference from: https://github.com/antirez/ds4/blob/main/ds4_agent.c#L668
@tool_registry.register(name="BetterEdit")
async def better_edit(
    path: str,
    old_string: str,
    new_string: str,
    ctx: ToolContext,
) -> str:
    """Edit a file using path, old_string, and new_string. The old text must match exactly once in the file; otherwise the edit fails for safety.

    For large replacements, prefer anchored old_string: write the first lines, then [upto], then the final lines.
    The tool replaces everything from the head through the tail. If the head or tail is ambiguous, the edit fails.

    After [upto], always write unique final lines before closing old_string; never close old_string immediately after [upto].
    Do not use a generic tail anchor like:

        some_function() {
            ...
    [upto]
        }

    because the closing brace may match many functions. Instead include final lines that are unique near that function,
    for example its last calculation and return line before the brace.

    Example anchored edit:

        old_string: "static int parse(void) {
            int ok = 0;
    [upto]
            return ok;
        }"
        new_string: "static int parse(void) {
            return parse_impl();
        }"

    To insert text, use old_string set to an exact unique anchor and new_string set to that anchor plus the added text.

    Without [upto], old_string must match exactly once.

    Args:
        path: Target file path.
        old_string: Exact text to find. Use [upto] marker for anchored edit.
        new_string: Replacement text.
    """
    # Read file
    try:
        with open(path, "r", encoding="utf-8") as f:
            content = f.read()
    except FileNotFoundError:
        raise BetterEditError(f"file not found: {path}")
    except (IsADirectoryError, PermissionError) as e:
        raise BetterEditError(f"cannot read {path}: {e.strerror}")

    if not old_string:
        raise BetterEditError("old_string cannot be empty")

    # Find span to replace
    start, end = _find_old_span(content, old_string)
    new_content = content[:start] + new_string + content[end:]

    # Atomic write
    tmp = f"{path}.tmp.{os.getpid()}"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(new_content)
        os.replace(tmp, path)
    except Exception as e:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise BetterEditError(f"write failed: {e}")

    # Emit DiffContentEvent for frontend rendering
    ctx.emit(
        DiffContentEvent(
            session_id=ctx.session_id,
            path=path,
            old_text=content,
            new_text=new_content,
            tool_call_id=current_tool_call_id() or "",
        )
    )

    # Calculate stats
    total_old_lines = len(content.splitlines())
    total_new_lines = len(new_content.splitlines())
    has_upto = UPTO_MARKER in old_string

    if has_upto:
        return (
            f"better_edit: ok (anchored)\n"
            f"  file: {total_old_lines} → {total_new_lines} lines"
        )
    else:
        old_lines = len(old_string.splitlines())
        new_lines = len(new_string.splitlines())
        line_no = content[:start].count("\n") + 1
        return (
            f"better_edit: ok @ line {line_no}\n"
            f"  replaced: {old_lines} → {new_lines} lines\n"
            f"  file: {total_old_lines} → {total_new_lines} lines"
        )
