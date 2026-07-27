"""Read / Write / Edit 远程工具。"""

from __future__ import annotations

import os
from pathlib import Path

from wing_sdk.tools._resolve import resolve_path as _resolve


async def read(
    path: str,
    offset: int = 1,
    limit: int = 2000,
    line_numbers: bool = False,
    _workspace: str = ".",
) -> str:
    """Read file segment.

    Args:
        path: File to read.
        offset: Line number to start from (1-based). Negative counts from end (-1 = last line).
        limit: Number of lines to read (capped at 2000).
        line_numbers: Prefix each line with its line number.

    Returns:
        File content with metadata header.
    """
    resolved = _resolve(path, _workspace)
    limit = min(max(limit, 1), 2000)

    if not os.path.exists(resolved):
        return f"read_file: {path}: No such file"
    if os.path.isdir(resolved):
        return f"read_file: {path}: Is a directory"

    try:
        text = Path(resolved).read_text(encoding="utf-8", errors="replace")
    except PermissionError:
        return f"read_file: {path}: Permission denied"

    lines = text.splitlines()
    total = len(lines)

    if offset < 0:
        start = max(0, total + offset)
    else:
        start = max(0, offset - 1)

    end = min(start + limit, total)
    segment = lines[start:end]

    header = f"[file: {path} | lines {start + 1}-{end}/{total}]"
    body_lines = []
    for i, line in enumerate(segment, start=start + 1):
        if line_numbers:
            body_lines.append(f"{i:>6}\t{line}")
        else:
            body_lines.append(line)

    result = header + "\n" + "\n".join(body_lines)
    if end < total:
        result += f"\n[... {total - end} more lines]"
    return result


async def write(path: str, content: str, _workspace: str = ".") -> str:
    """Write content to file (overwrite).

    Args:
        path: Target file path.
        content: Content to write.

    Returns:
        Success with stats, or error message.
    """
    resolved = _resolve(path, _workspace)

    try:
        parent = os.path.dirname(os.path.abspath(resolved))
        if parent:
            os.makedirs(parent, exist_ok=True)

        existed = os.path.exists(resolved)
        Path(resolved).write_text(content, encoding="utf-8")

        byte_count = len(content.encode("utf-8"))
        new_lines = len(content.splitlines())

        if existed:
            return f"write: ok (overwritten)\n  {new_lines} lines, {byte_count} bytes"
        return f"write: ok (created)\n  {new_lines} lines, {byte_count} bytes"
    except PermissionError:
        return f"write: cannot write '{path}': Permission denied"
    except IsADirectoryError:
        return f"write: cannot write '{path}': Is a directory"
    except OSError as e:
        return f"write: cannot write '{path}': {e.strerror}"


async def edit(
    path: str,
    old_block: str,
    new_block: str,
    replace_all: bool = False,
    _workspace: str = ".",
) -> str:
    """Replace old_block with new_block. Exact match only. replace_all=True replaces all matches.

    Args:
        path: Target file path.
        old_block: Exact text to find (must be unique in file unless replace_all=True).
        new_block: Replacement text.
        replace_all: Replace all matches instead of requiring uniqueness.

    Returns:
        Success with location and stats, or concise error with hints.
    """
    resolved = _resolve(path, _workspace)

    if not os.path.exists(resolved):
        return f"edit: {path}: No such file"

    try:
        text = Path(resolved).read_text(encoding="utf-8")
    except PermissionError:
        return f"edit: {path}: Permission denied"

    count = text.count(old_block)
    if count == 0:
        return f"edit: old_block not found in {path}"
    if count > 1 and not replace_all:
        return f"edit: old_block found {count} times in {path} (use replace_all=True)"

    new_text = (
        text.replace(old_block, new_block)
        if replace_all
        else text.replace(old_block, new_block, 1)
    )
    Path(resolved).write_text(new_text, encoding="utf-8")

    replaced = count if replace_all else 1
    return f"edit: ok ({replaced} replacement{'s' if replaced > 1 else ''})"
