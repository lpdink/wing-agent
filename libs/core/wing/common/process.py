# wing/common/process.py
"""Process group management utilities."""

import asyncio
import os
import signal


def kill_process_group(process: asyncio.subprocess.Process) -> None:
    """SIGKILL the entire process group (shell + all children).

    Subprocesses spawned with start_new_session=True live in a dedicated
    process group. process.kill() only kills the shell itself, leaving
    background children as orphans. killpg() cleans up the entire tree.
    """
    if process.pid is None:
        return
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        try:
            process.kill()
        except ProcessLookupError:
            pass
