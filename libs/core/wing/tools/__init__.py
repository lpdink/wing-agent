# wing/tools/__init__.py
"""Tool implementations for wing agent.

This package contains all tool implementations. Registration runs when the
package is imported (the decorators execute at import time) —— 谁 import
本包，谁就负责安装：顶层 `wing/__init__` 不再代劳导入（无 import 副作用），
由组合根 `WingRuntime.__init__` 与测试侧 `libs/core/tests/conftest.py`
显式 ``import wing.tools`` 完成注册。

Tools:
    - AskUserQuestion: Ask user for feedback during execution
    - Bash: Execute shell commands
    - Read: Read file segments
    - ReadImage: Read image files as media attachments
    - Write: Write files (overwrite)
    - Edit: Precise text replacement
    - Glob: File pattern matching
    - Grep: File content search
"""

from wing.tools.ask_user import ask_user
from wing.tools.bash import execute_shell
from wing.tools.file import edit_file, read_file, write_file
from wing.tools.read_image import read_image
from wing.tools.search import glob_files, grep_files
from wing.tools.todo import todo_write

__all__ = [
    "ask_user",
    "execute_shell",
    "read_file",
    "read_image",
    "write_file",
    "edit_file",
    "glob_files",
    "grep_files",
    "todo_write",
]
