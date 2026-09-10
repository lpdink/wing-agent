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
        raw = Path(resolved).read_bytes()
        st = os.stat(resolved)
    except PermissionError:
        return f"read_file: {path}: Permission denied"

    # 二进制探测与核心 Read 一致（首 8KB 含 NUL 即判定）
    if b"\x00" in raw[:8192]:
        import mimetypes

        mime, _ = mimetypes.guess_type(resolved)
        return f"read_file: {path}: Binary file ({mime or 'unknown'})"

    # 编码探测与核心 Read 一致：utf-8(-sig) 优先，退 latin-1
    try:
        text = raw.decode("utf-8-sig")
        encoding = "utf-8"
    except UnicodeDecodeError:
        text = raw.decode("latin-1", errors="replace")
        encoding = "latin-1"

    lines = text.splitlines()
    total = len(lines)

    if total == 0:
        return f"[file: {path} | empty | {encoding}]"

    if offset < 0:
        start = max(0, total + offset)
    else:
        start = min(max(offset - 1, 0), total)

    # mtime 供调用方做变更跟踪（与核心 Read 输出同构）
    mtime = int(st.st_mtime)
    if start >= total:
        return f"[file: {path} | offset {offset} beyond EOF ({total} lines) | {encoding} | mtime {mtime}]"

    end = min(start + limit, total)
    segment = lines[start:end]

    if line_numbers:
        segment = [f"{start + i + 1}→{line}" for i, line in enumerate(segment)]

    header = (
        f"[file: {path} | lines {start + 1}-{end}/{total} | {encoding} | mtime {mtime}]"
    )
    trailer = f"\n[... {total - end} more lines]" if end < total else ""
    return f"{header}\n" + "\n".join(segment) + trailer


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
    old_string: str,
    new_string: str,
    replace_all: bool = False,
    _workspace: str = ".",
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
    resolved = _resolve(path, _workspace)

    if not os.path.exists(resolved):
        return f"edit: {path}: No such file"

    try:
        text = Path(resolved).read_text(encoding="utf-8")
    except PermissionError:
        return f"edit: {path}: Permission denied"

    count = text.count(old_string)
    if count == 0:
        return f"edit: old_string not found in {path}"
    if count > 1 and not replace_all:
        return f"edit: old_string found {count} times in {path} (use replace_all=True)"

    new_text = (
        text.replace(old_string, new_string)
        if replace_all
        else text.replace(old_string, new_string, 1)
    )
    Path(resolved).write_text(new_text, encoding="utf-8")

    # 统计输出与核心 Edit 同构（位置 + 行数变化）
    total_old_lines = len(text.splitlines())
    total_new_lines = len(new_text.splitlines())

    if replace_all:
        return (
            f"edit: ok ({count} replacements)\n"
            f"  file: {total_old_lines} → {total_new_lines} lines"
        )

    pos = text.find(old_string)
    old_lines = len(old_string.splitlines())
    new_lines = len(new_string.splitlines())
    line_no = text[:pos].count("\n") + 1
    return (
        f"edit: ok @ line {line_no}\n"
        f"  replaced: {old_lines} → {new_lines} lines\n"
        f"  file: {total_old_lines} → {total_new_lines} lines"
    )
