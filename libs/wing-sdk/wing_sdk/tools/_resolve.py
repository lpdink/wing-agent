"""共享路径解析——workspace 沙箱约束。

注意：这不是安全边界（Bash 工具不受此限制），仅约束路径类工具
（Read/Write/Edit/Glob/Grep）不意外逃逸 workspace。
"""

from __future__ import annotations

import os


def resolve_path(path: str, workspace: str) -> str:
    """Resolve path within workspace. Rejects traversal outside."""
    if os.path.isabs(path):
        resolved = os.path.realpath(path)
    else:
        resolved = os.path.realpath(os.path.join(workspace, path))
    ws_real = os.path.realpath(workspace)
    if not resolved.startswith(ws_real + os.sep) and resolved != ws_real:
        raise ValueError(f"path escapes workspace: {path}")
    return resolved
