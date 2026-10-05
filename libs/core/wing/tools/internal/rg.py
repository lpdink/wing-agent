# wing/tools/internal/rg.py
"""ripgrep 调用封装 —— Glob / Grep 共用的子进程入口。"""

import subprocess
from pathlib import Path

from wing.schema import ToolError


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
