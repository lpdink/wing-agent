# wing/tools/builtin/edit.py
"""Edit tool: precise text replacement."""

import os

from wing.agent import ToolContext, current_tool_call_id
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.internal.diff_window import build_diff_events, find_all
from wing.tools.internal.utils import resolve_path as _resolve_path


@tool_registry.register(name="Edit")
async def edit_file(
    path: str,
    old_string: str,
    new_string: str,
    ctx: ToolContext,
    replace_all: bool = False,
) -> str:
    """Replace old_string with new_string. Exact match only. replace_all=True replaces all matches.

    Args:
        path: Target file path.
        old_string: Exact text to find (must be unique in file unless replace_all=True).
        new_string: Replacement text.
        replace_all: Replace all matches instead of requiring uniqueness.

    Returns:
        Success with location and stats, or concise error with hints.
    """
    path = _resolve_path(path, ctx)

    try:
        with open(path, "r", encoding="utf-8") as f:
            content = f.read()
    except FileNotFoundError:
        raise ToolError(f"edit: {path}: No such file")
    except (IsADirectoryError, PermissionError) as e:
        raise ToolError(f"edit: {path}: {e.strerror}")

    if not old_string:
        raise ToolError("edit: old_string cannot be empty")

    count = content.count(old_string)
    if count == 0:
        total_lines = len(content.splitlines())
        if total_lines == 0:
            raise ToolError("edit: old_string not found (file is empty)")
        first_lines = "\n".join(content.splitlines()[:3])
        raise ToolError(
            f"edit: old_string not found in {total_lines} lines\nfile starts with:\n{first_lines}"
        )
    if count > 1 and not replace_all:
        pos = 0
        match_lines = []
        while True:
            pos = content.find(old_string, pos)
            if pos == -1:
                break
            line_no = content[:pos].count("\n") + 1
            match_lines.append(line_no)
            pos += 1
        raise ToolError(
            f"edit: ambiguous ({count} matches) at lines: {', '.join(map(str, match_lines[:5]))} — use replace_all=True"
        )

    if replace_all:
        positions = find_all(content, old_string)
        new_content = content.replace(old_string, new_string)
    else:
        pos = content.find(old_string)
        positions = [pos]
        new_content = content[:pos] + new_string + content[pos + len(old_string) :]

    tmp = f"{path}.tmp.{os.getpid()}"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(new_content)
        os.replace(tmp, path)
    except Exception as e:
        os.unlink(tmp) if os.path.exists(tmp) else None
        raise ToolError(f"edit: write failed: {e}")

    # 成功：emit DiffContentEvent（每个匹配位置一个窗口，非整份文件）
    for event in build_diff_events(
        session_id=ctx.session_id,
        path=path,
        tool_call_id=current_tool_call_id() or "",
        old_content=content,
        new_content=new_content,
        old_len=len(old_string),
        new_len=len(new_string),
        positions=positions,
    ):
        ctx.emit(event)

    total_old_lines = len(content.splitlines())
    total_new_lines = len(new_content.splitlines())

    if replace_all:
        return (
            f"edit: ok ({count} replacements)\n"
            f"  file: {total_old_lines} → {total_new_lines} lines"
        )

    pos = positions[0]
    old_lines = len(old_string.splitlines())
    new_lines = len(new_string.splitlines())
    line_no = content[:pos].count("\n") + 1

    return (
        f"edit: ok @ line {line_no}\n"
        f"  replaced: {old_lines} → {new_lines} lines\n"
        f"  file: {total_old_lines} → {total_new_lines} lines"
    )
