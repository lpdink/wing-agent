# wing/tools/search.py
"""File search tools: Glob, Grep using ripgrep."""

import subprocess
from pathlib import Path

from wing.agent import WingAgent
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.utils import resolve_path


def _run_rg(args: list[str], cwd: str | Path) -> tuple[str, str, int]:
    """Run ripgrep command and return (stdout, stderr, return_code)."""
    try:
        result = subprocess.run(
            ["rg"] + args,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=60,
        )
        return result.stdout, result.stderr, result.returncode
    except FileNotFoundError:
        raise ToolError("ripgrep (rg) not found. Please install ripgrep.")
    except subprocess.TimeoutExpired:
        raise ToolError("ripgrep command timed out")


@tool_registry.register(name="Glob")
async def glob_files(
    pattern: str,
    path: str = ".",
    agent: WingAgent | None = None,
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
    base = Path(resolve_path(path, agent))
    if not base.exists():
        raise ToolError(f"glob: {path}: No such directory")

    args = ["--files", "--glob", pattern, "--hidden"]
    if not respect_gitignore:
        args.append("--no-ignore")

    stdout, stderr, code = _run_rg(args, cwd=base)

    if code != 0 or not stdout.strip():
        return "glob: no matches found"

    files = [f for f in stdout.strip().split("\n") if f]

    # Sort by mtime (newest first)
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


@tool_registry.register(name="Grep")
async def grep_files(
    pattern: str,
    path: str = ".",
    agent: WingAgent | None = None,
    glob: str = "*",
    output_mode: str = "files_with_matches",
    i: bool = False,
    head_limit: int = 100,
    respect_gitignore: bool = True,
    context: int = 0,
) -> str:
    """Search file contents with regex pattern.

    Args:
        pattern: The regular expression pattern to search for.
        path: Directory or file to search in. Defaults to current directory.
        glob: Glob pattern to filter files (e.g., "*.py", "*.ts"). Ignored if path is a file.
        output_mode: "files_with_matches" (default), "content", or "count".
        i: Case insensitive search.
        head_limit: Max results to return (default 100).
        respect_gitignore: Whether to respect ignore rules (.gitignore, .ignore, etc).
            Only effective in git repositories. Defaults to True.
        context: Number of lines to show before and after each match.
            Only works with output_mode="content". Default is 0.

    Returns:
        - files_with_matches: file paths containing the pattern
        - content: file:line content for each match (with context if specified)
        - count: file path and match count
    """
    base = Path(resolve_path(path, agent))
    if not base.exists():
        raise ToolError(f"grep: {path}: No such file or directory")

    # Determine work directory and target
    if base.is_file():
        work_dir = base.parent
        target = base.name
    else:
        work_dir = base
        target = "."

    args = ["--hidden"]

    # Case insensitive
    if i:
        args.append("-i")

    # Ignore rules
    if not respect_gitignore:
        args.append("--no-ignore")

    # Output mode
    if output_mode == "files_with_matches":
        args.append("-l")
    elif output_mode == "count":
        args.append("-c")

    # Context
    if output_mode == "content" and context > 0:
        args.append(f"-C{context}")

    # Glob filter (directory only)
    if glob != "*" and base.is_dir():
        args.extend(["--glob", glob])

    # Content mode options
    if output_mode == "content":
        args.append("-n")
        args.append("--max-columns=200")

    # Pattern and target (rg: rg [OPTIONS] PATTERN [PATH])
    args.extend(["-e", pattern, target])

    stdout, stderr, code = _run_rg(args, cwd=work_dir)

    # Regex error
    if code == 2 and "regex" in stderr.lower():
        raise ToolError(f"grep: invalid regex: {stderr.strip()}")

    # No matches
    if code == 1 or not stdout.strip():
        return "grep: no matches found"

    if code != 0:
        raise ToolError(f"grep: error: {stderr.strip()}")

    # Post-process output
    lines = stdout.strip().split("\n")

    # For single file search, rg outputs "line:content" without filename
    # We need to add filename for consistency
    is_single_file = base.is_file()

    def strip_prefix(line: str) -> str:
        return line[2:] if line.startswith("./") else line

    if output_mode == "count":
        # Format: "./file:count" → "file: count"
        # Single file: "count" → "filename: count"
        if is_single_file:
            lines = [f"{base.name}: {lines[0]}"]
        else:
            lines = [
                f"{strip_prefix(p)}: {c}"
                for p, c in (line.rsplit(":", 1) for line in lines)
            ]
    elif output_mode == "files_with_matches":
        # Single file: just filename (rg outputs filename)
        # Directory: strip "./" prefix
        lines = [strip_prefix(line) for line in lines]
    else:  # content
        # Single file: "line:content" → "filename:line:content"
        # Directory: "./file:line:content" → "file:line:content"
        if is_single_file:
            lines = [f"{base.name}:{line}" for line in lines]
        else:
            lines = [strip_prefix(line) for line in lines]

    # Apply head_limit
    if len(lines) > head_limit:
        return (
            "\n".join(lines[:head_limit]) + f"\n[... truncated at {head_limit} results]"
        )

    return "\n".join(lines)
