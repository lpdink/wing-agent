"""共享进程辅助（对齐核心 common/process.py）。

超时清理须杀整个进程组——shell/rg 派生的子进程树不会随父进程退出。
"""

from __future__ import annotations

import asyncio
import os
import signal


def kill_process_group(proc: asyncio.subprocess.Process) -> None:
    """杀整个进程组（与核心 tools/bash.py 的 kill_process_group 同构）。"""
    try:
        pgid = os.getpgid(proc.pid)  # type: ignore[arg-type]
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        # 进程已退出或无权限，fallback 杀单进程
        try:
            proc.kill()
        except ProcessLookupError:
            pass
