# wing/tools/builtin/read.py
"""Read tool: read file segments."""

import os
import stat

from wing.agent import ToolContext
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.internal.utils import resolve_path as _resolve_path


MAX_LINES_TO_READ = 2000


@tool_registry.register(name="Read")
async def read_file(
    path: str,
    ctx: ToolContext,
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
    path = _resolve_path(path, ctx)

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

        from wing.media import SUPPORTED_IMAGE_MIMES, sniff_image_mime

        mime, _ = mimetypes.guess_type(path)
        # 指引只给"确实是支持格式的图片"：以内容 magic bytes 为准；扩展名
        # 猜出的 mime 命中支持集合时也认（内容判不出但扩展名说是图片，
        # 让模型去 ReadImage 拿到明确的格式判定）。其它二进制保持原文案。
        is_image = sniff_image_mime(raw[:64]) is not None or (
            mime is not None and mime in SUPPORTED_IMAGE_MIMES
        )
        hint = " — use ReadImage to view images" if is_image else ""
        raise ToolError(f"read_file: {path}: Binary file ({mime or 'unknown'}){hint}")

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
