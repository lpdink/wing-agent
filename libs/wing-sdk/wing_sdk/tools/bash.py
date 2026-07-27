"""Bash 远程工具。"""

from __future__ import annotations

import asyncio
import os
import signal


async def bash(command: str, timeout: int = 30, _workspace: str = ".") -> str:
    """Execute a shell command.

    Args:
        command: The command to execute.
        timeout: Maximum wait time in seconds.

    Returns:
        Command output with exit code.
    """
    try:
        proc = await asyncio.create_subprocess_shell(
            command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=_workspace,
            start_new_session=True,  # 新进程组，超时时杀整棵树
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except asyncio.TimeoutError:
        _kill_process_group(proc)  # type: ignore[arg-type]
        await proc.wait()  # type: ignore[union-attr]
        return f"exit code: -1 (timeout after {timeout}s)\ncommand timed out"
    except Exception as e:
        return f"exit code: -1\nerror: {e}"

    out = stdout.decode(errors="replace").strip()
    err = stderr.decode(errors="replace").strip()

    parts: list[str] = []
    if out:
        parts.append(out)
    if err:
        parts.append(f"[stderr]\n{err}")
    parts.append(f"exit code: {proc.returncode}")
    return "\n".join(parts)


def _kill_process_group(proc: asyncio.subprocess.Process) -> None:
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
