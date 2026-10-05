# wing/tools/builtin/glob.py
"""Glob tool: file pattern matching (ripgrep)."""

from pathlib import Path

from wing.agent import ToolContext
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.internal.rg import _run_rg
from wing.tools.internal.utils import resolve_path


@tool_registry.register(name="Glob")
async def glob_files(
    pattern: str,
    path: str = ".",
    ctx: ToolContext | None = None,
    respect_gitignore: bool = True,
) -> str:
    """Find files matching glob pattern.

    Supports glob patterns like "**/*.py" or "src/**/*.ts".
    Returns matching file paths sorted by modification time (newest first).

    Args:
        pattern: The glob pattern to match files against (e.g., "**/*.py").
        path: The directory to search in. Defaults to current directory.
        respect_gitignore: Whether to respect ignore rules (.gitignore, .ignore, etc).
            Only effective in git repositories. Defaults to True.

    Returns:
        List of matching file paths, one per line.
    """
    base = Path(resolve_path(path, ctx))
    if not base.exists():
        raise ToolError(f"glob: {path}: No such directory")

    args = ["--files", "--glob", pattern, "--hidden"]
    if not respect_gitignore:
        args.append("--no-ignore")

    stdout, stderr, code = _run_rg(args, cwd=base)

    if code != 0 or not stdout.strip():
        return "glob: no matches found"

    files = [f for f in stdout.strip().split("\n") if f]

    try:
        files_with_mtime = [
            (f, (base / f).stat().st_mtime) for f in files if (base / f).is_file()
        ]
        files_with_mtime.sort(key=lambda x: x[1], reverse=True)
        files = [f[0] for f in files_with_mtime]
    except OSError:
        pass

    max_results = 100
    if len(files) > max_results:
        return (
            "\n".join(files[:max_results])
            + f"\n[... {len(files) - max_results} more files]"
        )

    return "\n".join(files)
