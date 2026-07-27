"""Bash 远程工具。"""

from __future__ import annotations

import asyncio


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
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except asyncio.TimeoutError:
        proc.kill()  # type: ignore[union-attr]
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
