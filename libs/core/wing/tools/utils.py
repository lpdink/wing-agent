import os
from pathlib import Path

from wing.agent import ToolContext


def resolve_path(path: str, ctx: ToolContext | None = None) -> str:
    """Resolve a path relative to the ctx's cwd, falling back to process cwd.

    Shared by file/search tools so relative paths resolve against the session
    workspace directory.
    """
    if os.path.isabs(path):
        return path
    if ctx is not None:
        cwd = ctx.cwd
        if cwd is not None:
            return str(Path(cwd) / path)
    return os.path.abspath(path)
