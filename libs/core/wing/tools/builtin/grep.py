# wing/tools/builtin/grep.py
"""Grep tool: file content search (ripgrep)."""

from pathlib import Path

from wing.agent import ToolContext
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.internal.rg import _run_rg
from wing.tools.internal.utils import resolve_path


@tool_registry.register(name="Grep")
async def grep_files(
    pattern: str,
    path: str = ".",
    ctx: ToolContext | None = None,
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
    base = Path(resolve_path(path, ctx))
    if not base.exists():
        raise ToolError(f"grep: {path}: No such file or directory")

    if base.is_file():
        work_dir = base.parent
        target = base.name
    else:
        work_dir = base
        target = "."

    args = ["--hidden"]

    if i:
        args.append("-i")

    if not respect_gitignore:
        args.append("--no-ignore")

    if output_mode == "files_with_matches":
        args.append("-l")
    elif output_mode == "count":
        args.append("-c")

    if output_mode == "content" and context > 0:
        args.append(f"-C{context}")

    if glob != "*" and base.is_dir():
        args.extend(["--glob", glob])

    if output_mode == "content":
        args.append("-n")
        args.append("--max-columns=200")

    args.extend(["-e", pattern, target])

    stdout, stderr, code = _run_rg(args, cwd=work_dir)

    if code == 2 and "regex" in stderr.lower():
        raise ToolError(f"grep: invalid regex: {stderr.strip()}")

    if code == 1 or not stdout.strip():
        return "grep: no matches found"

    if code != 0:
        raise ToolError(f"grep: error: {stderr.strip()}")

    lines = stdout.strip().split("\n")

    # For single file search, rg outputs "line:content" without filename
    # We need to add filename for consistency
    is_single_file = base.is_file()

    def strip_prefix(line: str) -> str:
        return line[2:] if line.startswith("./") else line

    if output_mode == "count":
        if is_single_file:
            lines = [f"{base.name}: {lines[0]}"]
        else:
            lines = [
                f"{strip_prefix(p)}: {c}"
                for p, c in (line.rsplit(":", 1) for line in lines)
            ]
    elif output_mode == "files_with_matches":
        lines = [strip_prefix(line) for line in lines]
    else:  # content
        if is_single_file:
            lines = [f"{base.name}:{line}" for line in lines]
        else:
            lines = [strip_prefix(line) for line in lines]

    if len(lines) > head_limit:
        return (
            "\n".join(lines[:head_limit]) + f"\n[... truncated at {head_limit} results]"
        )

    return "\n".join(lines)
