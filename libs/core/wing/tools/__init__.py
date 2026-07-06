# wing/tools/__init__.py
"""Tool implementations for wing agent.

This package contains all tool implementations, automatically registered
with the tool_registry on import.

Tools:
    - AskUserQuestion: Ask user for feedback during execution
    - Bash: Execute shell commands
    - BetterEdit: Edit with [upto] anchored edit support (experimental)
    - Read: Read file segments
    - Write: Write files (overwrite)
    - Edit: Precise text replacement
    - Glob: File pattern matching
    - Grep: File content search
"""

from wing.tools.ask_user import ask_user
from wing.tools.bash import execute_shell
from wing.tools.explorer import explorer_agent
from wing.tools.experimental import better_edit
from wing.tools.file import edit_file, read_file, write_file
from wing.tools.search import glob_files, grep_files
from wing.tools.todo import todo_write

__all__ = [
    "ask_user",
    "better_edit",
    "execute_shell",
    "explorer_agent",
    "read_file",
    "write_file",
    "edit_file",
    "glob_files",
    "grep_files",
    "todo_write",
]
