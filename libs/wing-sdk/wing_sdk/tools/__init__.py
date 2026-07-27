"""标准远程工具——Bash, Read, Write, Edit, Glob, Grep。

schema 与核心内置工具一致。通过 register_standard_tools() 一键注册到 ToolHost。
"""

from __future__ import annotations

import functools
from typing import TYPE_CHECKING, Any

from wing_sdk.schema import ToolParam
from wing_sdk.tools.bash import bash
from wing_sdk.tools.file import edit, read, write
from wing_sdk.tools.search import glob, grep

if TYPE_CHECKING:
    from wing_sdk.host import ToolHost

# ── Schema 定义（与核心 to_openai() 输出一致）──────────────────

_BASH_PARAMS = [
    ToolParam(name="command", type="string", description="The command to execute."),
    ToolParam(
        name="timeout",
        type="integer",
        description="Maximum wait time in seconds.",
        default=30,
    ),
]

_READ_PARAMS = [
    ToolParam(name="path", type="string", description="File to read."),
    ToolParam(
        name="offset",
        type="integer",
        description="Line number to start from (1-based). Negative counts from end (-1 = last line).",
        default=1,
    ),
    ToolParam(
        name="limit",
        type="integer",
        description="Number of lines to read (capped at 2000).",
        default=2000,
    ),
    ToolParam(
        name="line_numbers",
        type="boolean",
        description="Prefix each line with its line number.",
        default=False,
    ),
]

_WRITE_PARAMS = [
    ToolParam(name="path", type="string", description="Target file path."),
    ToolParam(name="content", type="string", description="Content to write."),
]

_EDIT_PARAMS = [
    ToolParam(name="path", type="string", description="Target file path."),
    ToolParam(
        name="old_block",
        type="string",
        description="Exact text to find (must be unique in file unless replace_all=True).",
    ),
    ToolParam(name="new_block", type="string", description="Replacement text."),
    ToolParam(
        name="replace_all",
        type="boolean",
        description="Replace all matches instead of requiring uniqueness.",
        default=False,
    ),
]

_GLOB_PARAMS = [
    ToolParam(
        name="pattern",
        type="string",
        description='The glob pattern to match files against (e.g., "**/*.py").',
    ),
    ToolParam(
        name="path",
        type="string",
        description="The directory to search in. Defaults to current directory.",
        default=".",
    ),
    ToolParam(
        name="respect_gitignore",
        type="boolean",
        description="Whether to respect ignore rules (.gitignore, .ignore, etc). Only effective in git repositories. Defaults to True.",
        default=True,
    ),
]

_GREP_PARAMS = [
    ToolParam(
        name="pattern",
        type="string",
        description="The regular expression pattern to search for.",
    ),
    ToolParam(
        name="path",
        type="string",
        description="Directory or file to search in. Defaults to current directory.",
        default=".",
    ),
    ToolParam(
        name="glob",
        type="string",
        description='Glob pattern to filter files (e.g., "*.py", "*.ts"). Ignored if path is a file.',
        default="*",
    ),
    ToolParam(
        name="output_mode",
        type="string",
        description='"files_with_matches" (default), "content", or "count".',
        default="files_with_matches",
    ),
    ToolParam(
        name="i",
        type="boolean",
        description="Case insensitive search.",
        default=False,
    ),
    ToolParam(
        name="head_limit",
        type="integer",
        description="Max results to return (default 100).",
        default=100,
    ),
    ToolParam(
        name="respect_gitignore",
        type="boolean",
        description="Whether to respect ignore rules (.gitignore, .ignore, etc). Only effective in git repositories. Defaults to True.",
        default=True,
    ),
    ToolParam(
        name="context",
        type="integer",
        description='Number of lines to show before and after each match. Only works with output_mode="content". Default is 0.',
        default=0,
    ),
]


def register_standard_tools(host: "ToolHost", workspace: str = ".") -> None:
    """将六个标准工具注册到 ToolHost，绑定 workspace。"""
    from wing_sdk.schema import RemoteToolSpec

    def _bind(fn: Any, ws: str) -> Any:
        """绑定 _workspace 参数。"""

        @functools.wraps(fn)
        async def wrapper(**kwargs: Any) -> str:
            return await fn(**kwargs, _workspace=ws)

        return wrapper

    tools = [
        (
            "Bash",
            "Execute a shell command.\n\nArgs:\n    command: The command to execute.\n    timeout: Maximum wait time in seconds.\n\nReturns:\n    Command output with exit code.",
            _BASH_PARAMS,
            bash,
        ),
        (
            "Read",
            'Read file segment.\n\nArgs:\n    path: File to read.\n    offset: Line number to start from (1-based). Negative counts from end (-1 = last line).\n    limit: Number of lines to read (capped at 2000).\n    line_numbers: Prefix each line with its line number.\n\nReturns:\n    [file: PATH | lines START-END/TOTAL | ENCODING | mtime MTIME]\n    Content...\n    [... N more lines]  # if truncated\n\nErrors: "read_file: PATH: No such file|Is a directory|Permission denied|Binary file"',
            _READ_PARAMS,
            read,
        ),
        (
            "Write",
            "Write content to file (overwrite).\n\nArgs:\n    path: Target file path.\n    content: Content to write.\n\nReturns:\n    Success with stats, or error message.",
            _WRITE_PARAMS,
            write,
        ),
        (
            "Edit",
            "Replace old_block with new_block. Exact match only. replace_all=True replaces all matches.\n\nArgs:\n    path: Target file path.\n    old_block: Exact text to find (must be unique in file unless replace_all=True).\n    new_block: Replacement text.\n    replace_all: Replace all matches instead of requiring uniqueness.\n\nReturns:\n    Success with location and stats, or concise error with hints.",
            _EDIT_PARAMS,
            edit,
        ),
        (
            "Glob",
            'Find files matching glob pattern.\n\nSupports glob patterns like "**/*.py" or "src/**/*.ts".\nReturns matching file paths sorted by modification time (newest first).\n\nArgs:\n    pattern: The glob pattern to match files against (e.g., "**/*.py").\n    path: The directory to search in. Defaults to current directory.\n    respect_gitignore: Whether to respect ignore rules (.gitignore, .ignore, etc).\n        Only effective in git repositories. Defaults to True.\n\nReturns:\n    List of matching file paths, one per line.',
            _GLOB_PARAMS,
            glob,
        ),
        (
            "Grep",
            'Search file contents with regex pattern.\n\nArgs:\n    pattern: The regular expression pattern to search for.\n    path: Directory or file to search in. Defaults to current directory.\n    glob: Glob pattern to filter files (e.g., "*.py", "*.ts"). Ignored if path is a file.\n    output_mode: "files_with_matches" (default), "content", or "count".\n    i: Case insensitive search.\n    head_limit: Max results to return (default 100).\n    respect_gitignore: Whether to respect ignore rules (.gitignore, .ignore, etc).\n        Only effective in git repositories. Defaults to True.\n    context: Number of lines to show before and after each match.\n        Only works with output_mode="content". Default is 0.\n\nReturns:\n    - files_with_matches: file paths containing the pattern\n    - content: file:line content for each match (with context if specified)\n    - count: file path and match count',
            _GREP_PARAMS,
            grep,
        ),
    ]

    for name, desc, params, fn in tools:
        spec = RemoteToolSpec(name=name, description=desc, params=params)
        host.register_spec(spec, _bind(fn, workspace))
