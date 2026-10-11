# wing/tools/experimental/read.py
"""HashlineRead——带内容指纹的读取（experimental 命名域）。

HashlineEdit 需要的 ``TAG`` 由本工具随内容一起给出：头部形如
``[hashline: PATH#TAG | ...]``，正文是 ``N→text`` 行号行——行号即 HashlineEdit
定位子使用的原文快照行号。读取语义（offset / limit / 2000 行上限）沿用主 Read。

为什么另起一个读工具而不是改主 Read：TAG 必须与内容**一起**到达模型（模型只能
引用它真正看过的版本），这是整套锚点机制的前提；实验期不把这条链路接进主工具。
"""

from __future__ import annotations

import os
import stat

from wing.agent import ToolContext
from wing.schema import ToolError, ToolParam
from wing.tool_registry import tool_registry
from wing.tools.experimental.hashline import content_tag, decode_utf8
from wing.tools.internal.utils import resolve_path

MAX_LINES = 2000

_PARAMS = [
    ToolParam(name="path", type="string", description="File to read."),
    ToolParam(
        name="offset",
        type="integer",
        description="Line number to start from (1-based). Negative counts from end.",
        default=1,
    ),
    ToolParam(
        name="limit",
        type="integer",
        description=f"Number of lines to read (capped at {MAX_LINES}).",
        default=MAX_LINES,
    ),
]


@tool_registry.register(
    name="Read",
    namespace="experimental",
    llm_name="HashlineRead",
    params=_PARAMS,
)
async def hashline_read(
    path: str,
    ctx: ToolContext,
    offset: int = 1,
    limit: int = MAX_LINES,
) -> str:
    """Read a file segment together with its content tag.

    Returns:
        [hashline: PATH#TAG | lines START-END/TOTAL | utf-8 | mtime MTIME]
        1→line one
        2→line two
        [... N more lines]  # if truncated

    Copy TAG into a patch section header ([PATH#TAG]) for HashlineEdit; a patch
    is rejected when the file changed after this read. Re-read after edits to
    get the new TAG (edit results also carry it).

    Errors: "hashline: PATH: No such file|Is a directory|Permission denied|not UTF-8|binary file"
    """
    limit = min(max(limit, 1), MAX_LINES)
    resolved = resolve_path(path, ctx)

    try:
        st = os.stat(resolved)
        if stat.S_ISDIR(st.st_mode):
            raise ToolError(f"hashline: {resolved}: Is a directory")
    except FileNotFoundError:
        raise ToolError(f"hashline: {resolved}: No such file")
    except PermissionError:
        raise ToolError(f"hashline: {resolved}: Permission denied")

    try:
        with open(resolved, "rb") as f:
            raw = f.read()
    except PermissionError:
        raise ToolError(f"hashline: {resolved}: Permission denied")

    if b"\x00" in raw[:8192]:
        raise ToolError(f"hashline: {resolved}: binary file — use Read or ReadImage")

    try:
        text, _had_bom = decode_utf8(raw)
    except UnicodeDecodeError as e:
        raise ToolError(f"hashline: {resolved}: not UTF-8 ({e}) — use Read")

    tag = content_tag(text)
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines = lines[:-1]
    total = len(lines)
    mtime = int(st.st_mtime)

    if total == 0:
        return f"[hashline: {resolved}#{tag} | empty | utf-8 | mtime {mtime}]"

    if offset < 0:
        start = max(0, total + offset)
    else:
        start = min(max(offset - 1, 0), total)

    if start >= total:
        return (
            f"[hashline: {resolved}#{tag} | offset {offset} beyond EOF ({total} lines) "
            f"| utf-8 | mtime {mtime}]"
        )

    end = min(start + limit, total)
    numbered = [f"{start + i + 1}→{line}" for i, line in enumerate(lines[start:end])]
    header = (
        f"[hashline: {resolved}#{tag} | lines {start + 1}-{end}/{total} "
        f"| utf-8 | mtime {mtime}]"
    )
    trailer = f"\n[... {total - end} more lines]" if end < total else ""
    return f"{header}\n" + "\n".join(numbered) + trailer
