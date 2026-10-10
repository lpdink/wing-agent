# wing/tools/builtin/write.py
"""Write tool: write files (overwrite)."""

import errno
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

        # 写入目标语义（对齐就地写 `open(path, "w")` 的可见行为）：
        # - 符号链接：写穿到真实目标（原子替换会把链接本身换成普通文件，
        #   真实目标静默留着旧内容——写"去哪了"与模型的理解错位）；
        # - FIFO / 设备节点等非常规文件：就地写（rename 会把节点本身换掉，
        #   消费者永远收不到字节）；
        # - 既有但不可写的文件：拒绝（只有目录写权限是 rename 的要求，
        #   目标自身的只读位拦不住替换——旧的 PermissionError 语义要显式补回）。
        target = Path(path)
        if target.is_symlink():
            target = Path(os.path.realpath(target))

        if target.exists() and not target.is_file() and not target.is_dir():
            with open(target, "w", encoding="utf-8") as f:
                f.write(content)
        else:
            if target.is_file() and not os.access(target, os.W_OK):
                # 只读目标：rename 只需要目录写权限，替换会绕过目标自身的
                # 只读位——显式拒绝，保住就地写时代的 PermissionError 语义。
                raise PermissionError(errno.EACCES, "Permission denied", str(path))
            # 原子替换（tmp + os.replace，与 Edit 同语义）：写入中途被杀不会
            # 留下半截文件（原内容要么完整保留、要么整体被替换），并发读者
            # 也不会读到部分内容。tmp 与目标同目录（同文件系统）、fsync 先于
            # rename（断电后不会出现"已改名、内容为空"）。父目录自动创建。
            atomic_write_text(target, content)

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
