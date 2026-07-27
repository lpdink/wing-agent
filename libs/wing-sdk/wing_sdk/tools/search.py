"""Glob / Grep 远程工具。"""

from __future__ import annotations

import asyncio
import os
from pathlib import Path


async def glob(
    pattern: str,
    path: str = ".",
    respect_gitignore: bool = True,
    _workspace: str = ".",
) -> str:
    """Find files matching glob pattern.

    Supports glob patterns like "**/*.py" or "src/**/*.ts".
    Returns matching file paths sorted by modification time (newest first).

    Args:
        pattern: The glob pattern to match files against (e.g., "**/*.py").
        path: The directory to search in. Defaults to current directory.
        respect_gitignore: Whether to respect ignore rules (.gitignore, .ignore, etc).

    Returns:
        List of matching file paths, one per line.
    """
    base = _resolve(path, _workspace)
    if not os.path.isdir(base):
        return f"glob: {path}: No such directory"

    args = ["--files", "--glob", pattern, "--hidden"]
    if not respect_gitignore:
        args.append("--no-ignore")

    stdout, code = await _run_rg(args, cwd=base)
    if code != 0 or not stdout.strip():
        return "glob: no matches found"

    files = [f for f in stdout.strip().split("\n") if f]

    # Sort by mtime (newest first)
    try:
        files_with_mtime = [
            (f, (Path(base) / f).stat().st_mtime)
            for f in files
            if (Path(base) / f).is_file()
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


async def grep(
    pattern: str,
    path: str = ".",
    glob_filter: str = "*",
    output_mode: str = "files_with_matches",
    i: bool = False,
    head_limit: int = 100,
    respect_gitignore: bool = True,
    context: int = 0,
    _workspace: str = ".",
) -> str:
    """Search file contents with regex pattern.

    Args:
        pattern: The regular expression pattern to search for.
        path: Directory or file to search in. Defaults to current directory.
        glob_filter: Glob pattern to filter files (e.g., "*.py", "*.ts").
        output_mode: "files_with_matches" (default), "content", or "count".
        i: Case insensitive search.
        head_limit: Max results to return (default 100).
        respect_gitignore: Whether to respect ignore rules (.gitignore, .ignore, etc).
        context: Number of lines to show before and after each match.

    Returns:
        Search results depending on output_mode.
    """
    base = _resolve(path, _workspace)
    if not os.path.exists(base):
        return f"grep: {path}: No such file or directory"

    if os.path.isfile(base):
        work_dir = os.path.dirname(base) or "."
        target = os.path.basename(base)
    else:
        work_dir = base
        target = "."

    args = ["--hidden"]
    if not respect_gitignore:
        args.append("--no-ignore")
    if i:
        args.append("-i")
    if glob_filter and glob_filter != "*":
        args.extend(["--glob", glob_filter])

    if output_mode == "files_with_matches":
        args.append("--files-with-matches")
    elif output_mode == "count":
        args.append("--count")
    else:  # content
        args.append("--line-number")
        if context > 0:
            args.extend(["-C", str(context)])

    args.extend([pattern, target])
    stdout, code = await _run_rg(args, cwd=work_dir)

    if code != 0 or not stdout.strip():
        return "grep: no matches found"

    lines = stdout.strip().split("\n")
    if len(lines) > head_limit:
        return (
            "\n".join(lines[:head_limit])
            + f"\n[... {len(lines) - head_limit} more results]"
        )
    return stdout.strip()


async def _run_rg(args: list[str], cwd: str) -> tuple[str, int]:
    """Run ripgrep, fallback to find/grep if rg unavailable."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "rg",
            *args,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=cwd,
        )
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=30)
        return stdout.decode(errors="replace"), proc.returncode or 0
    except FileNotFoundError:
        # rg not installed — minimal fallback
        return "grep: ripgrep not available in this environment", 1
    except asyncio.TimeoutError:
        return "grep: timeout", 1


def _resolve(path: str, workspace: str) -> str:
    """Resolve path within workspace sandbox. Rejects traversal outside."""
    if os.path.isabs(path):
        resolved = os.path.realpath(path)
    else:
        resolved = os.path.realpath(os.path.join(workspace, path))
    ws_real = os.path.realpath(workspace)
    if not resolved.startswith(ws_real + os.sep) and resolved != ws_real:
        raise ValueError(f"path escapes workspace: {path}")
    return resolved
