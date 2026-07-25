# wing/tools/file.py
"""File operation tools: Read, Write, Edit."""

import os
import stat
from pathlib import Path

from wing.agent import WingAgent
from wing.event import DiffContentEvent
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.utils import resolve_path as _resolve_path


@tool_registry.register(name="Write")
async def write_file(path: str, content: str, agent: WingAgent) -> str:
    """Write content to file (overwrite).

    Args:
        path: Target file path.
        content: Content to write.

    Returns:
        Success with stats, or error message.
    """
    try:
        path = _resolve_path(path, agent)

        # Ensure parent directory exists
        parent = os.path.dirname(os.path.abspath(path))
        if parent and not os.path.exists(parent):
            os.makedirs(parent, exist_ok=True)

        # Read old content BEFORE writing (for diff event)
        old_text: str | None = None
        existed = os.path.exists(path)
        old_lines = 0
        if existed:
            try:
                old_text = Path(path).read_text()
                old_lines = len(old_text.splitlines())
            except Exception:
                pass

        with open(path, "w", encoding="utf-8") as f:
            f.write(content)

        # Stats
        byte_count = len(content.encode("utf-8"))
        new_lines = len(content.splitlines())

        # Emit DiffContentEvent for frontend diff rendering
        agent.emit(
            DiffContentEvent(
                session_id=agent.session_id,
                path=path,
                old_text=old_text,
                new_text=content,
            )
        )

        if existed:
            return f"write: ok (overwritten)\n  {old_lines} → {new_lines} lines, {byte_count} bytes"
        else:
            return f"write: ok (created)\n  {new_lines} lines, {byte_count} bytes"

    except PermissionError:
        raise ToolError(f"write: cannot write '{path}': Permission denied")
    except IsADirectoryError:
        raise ToolError(f"write: cannot write '{path}': Is a directory")
    except OSError as e:
        raise ToolError(f"write: cannot write '{path}': {e.strerror}")
    except Exception as e:
        raise ToolError(f"write: error writing '{path}': {str(e)}")


@tool_registry.register(name="Edit")
async def edit_file(
    path: str,
    old_block: str,
    new_block: str,
    agent: WingAgent,
    replace_all: bool = False,
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
    path = _resolve_path(path, agent)

    # 读取
    try:
        with open(path, "r", encoding="utf-8") as f:
            content = f.read()
    except FileNotFoundError:
        raise ToolError(f"edit: {path}: No such file")
    except (IsADirectoryError, PermissionError) as e:
        raise ToolError(f"edit: {path}: {e.strerror}")

    if not old_block:
        raise ToolError("edit: old_block cannot be empty")

    # 计数匹配
    count = content.count(old_block)
    if count == 0:
        # 提供有用提示：文件总行数、是否为空文件
        total_lines = len(content.splitlines())
        if total_lines == 0:
            raise ToolError("edit: old_block not found (file is empty)")
        # 提供前几行内容作为提示
        first_lines = "\n".join(content.splitlines()[:3])
        raise ToolError(
            f"edit: old_block not found in {total_lines} lines\nfile starts with:\n{first_lines}"
        )
    if count > 1 and not replace_all:
        # 返回所有匹配位置的行号
        pos = 0
        match_lines = []
        while True:
            pos = content.find(old_block, pos)
            if pos == -1:
                break
            line_no = content[:pos].count("\n") + 1
            match_lines.append(line_no)
            pos += 1
        raise ToolError(
            f"edit: ambiguous ({count} matches) at lines: {', '.join(map(str, match_lines[:5]))} — use replace_all=True"
        )

    # 原子替换
    if replace_all:
        new_content = content.replace(old_block, new_block)
    else:
        pos = content.find(old_block)
        new_content = content[:pos] + new_block + content[pos + len(old_block) :]

    tmp = f"{path}.tmp.{os.getpid()}"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(new_content)
        os.replace(tmp, path)
    except Exception as e:
        os.unlink(tmp) if os.path.exists(tmp) else None
        raise ToolError(f"edit: write failed: {e}")

    # 成功：emit DiffContentEvent（传完整文件内容，非仅变更块）
    agent.emit(
        DiffContentEvent(
            session_id=agent.session_id,
            path=path,
            old_text=content,
            new_text=new_content,
        )
    )

    # 计算统计信息
    total_old_lines = len(content.splitlines())
    total_new_lines = len(new_content.splitlines())

    if replace_all:
        return (
            f"edit: ok ({count} replacements)\n"
            f"  file: {total_old_lines} → {total_new_lines} lines"
        )

    pos = content.find(old_block)
    old_lines = len(old_block.splitlines())
    new_lines = len(new_block.splitlines())
    line_no = content[:pos].count("\n") + 1

    # 轻量级返回：位置 + 行数变化 + 文件变化
    return (
        f"edit: ok @ line {line_no}\n"
        f"  replaced: {old_lines} → {new_lines} lines\n"
        f"  file: {total_old_lines} → {total_new_lines} lines"
    )


MAX_LINES_TO_READ = 2000


@tool_registry.register(name="Read")
async def read_file(
    path: str,
    agent: WingAgent,
    offset: int = 1,
    limit: int = MAX_LINES_TO_READ,
    line_numbers: bool = False,
) -> str:
    """Read file segment.

    Args:
        path: File to read.
        offset: Line number to start from (1-based). Negative counts from end (-1 = last line).
        limit: Number of lines to read (capped at 2000).
        line_numbers: Prefix each line with its line number.

    Returns:
        [file: PATH | lines START-END/TOTAL | ENCODING | mtime MTIME]
        Content...
        [... N more lines]  # if truncated

    Errors: "read_file: PATH: No such file|Is a directory|Permission denied|Binary file"
    """
    limit = min(max(limit, 1), MAX_LINES_TO_READ)
    path = _resolve_path(path, agent)

    try:
        st = os.stat(path)
        if stat.S_ISDIR(st.st_mode):
            raise ToolError(f"read_file: {path}: Is a directory")
    except FileNotFoundError:
        raise ToolError(f"read_file: {path}: No such file")
    except PermissionError:
        raise ToolError(f"read_file: {path}: Permission denied")

    try:
        with open(path, "rb") as f:
            raw = f.read()
    except Exception as e:
        raise ToolError(f"read_file: {path}: {e}")

    if b"\x00" in raw[:8192]:
        import mimetypes

        mime, _ = mimetypes.guess_type(path)
        raise ToolError(f"read_file: {path}: Binary file ({mime or 'unknown'})")

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

    # Convert 1-based offset to 0-based index; negative offsets count from end
    if offset < 0:
        start = max(0, total + offset)
    else:
        start = min(max(offset - 1, 0), total)

    if start >= total:
        mtime = int(st.st_mtime)
        return f"[file: {path} | offset {offset} beyond EOF ({total} lines) | {encoding} | mtime {mtime}]"

    end = min(start + limit, total)

    selected = lines[start:end]

    if line_numbers:
        selected = [f"{start + i + 1}→{line}" for i, line in enumerate(selected)]

    # Include mtime for change tracking
    mtime = int(st.st_mtime)
    header = (
        f"[file: {path} | lines {start + 1}-{end}/{total} | {encoding} | mtime {mtime}]"
    )
    trailer = f"\n[... {total - end} more lines]" if end < total else ""

    return f"{header}\n" + "\n".join(selected) + trailer
