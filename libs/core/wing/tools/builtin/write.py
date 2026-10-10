# wing/tools/builtin/write.py
"""Write tool: write files (overwrite)."""

import os
from pathlib import Path

from wing.agent import ToolContext, current_tool_call_id
from wing.common.fs import atomic_write_text
from wing.event import DiffContentEvent
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.internal.utils import resolve_path as _resolve_path


@tool_registry.register(name="Write")
async def write_file(path: str, content: str, ctx: ToolContext) -> str:
    """Write content to file (overwrite).

    Args:
        path: Target file path.
        content: Content to write.

    Returns:
        Success with stats, or error message.
    """
    try:
        path = _resolve_path(path, ctx)

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

        # 原子替换（tmp + os.replace，与 Edit 同语义）：写入中途被杀不会留下
        # 半截文件（原内容要么完整保留、要么整体被替换），并发读者也不会读到
        # 部分内容。tmp 与目标同目录（同文件系统）、fsync 先于 rename（断电后
        # 不会出现"已改名、内容为空"）。父目录自动创建。
        atomic_write_text(Path(path), content)

        byte_count = len(content.encode("utf-8"))
        new_lines = len(content.splitlines())

        # Emit DiffContentEvent for frontend diff rendering.
        # Write keeps the full payload (not windowed): `old_text=None` for a
        # new file (all green), the whole old content on overwrite. Both
        # windows start at line 1, so the default start lines apply.
        ctx.emit(
            DiffContentEvent(
                session_id=ctx.session_id,
                path=path,
                old_text=old_text,
                new_text=content,
                tool_call_id=current_tool_call_id() or "",
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
