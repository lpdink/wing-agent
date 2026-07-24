# wing/tools/bash.py
"""Shell command execution tool — stateless, no cwd persistence."""

import asyncio
import time

from wing.agent import WingAgent
from wing.common.logger import log
from wing.common.process import kill_process_group
from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.shell_safety import is_dangerous_command

# Feedback timeout in seconds
FEEDBACK_TIMEOUT = 6000


@tool_registry.register(name="Bash", add_purpose=True)
async def execute_shell(command: str, agent: WingAgent, timeout: int = 30) -> str:
    """Execute a shell command.

    Args:
        command: The command to execute.
        timeout: Maximum wait time in seconds.

    Returns:
        Command output with exit code.
    """
    # Skip safety check when yolo is enabled (priority: runtime > agent > global)
    if not agent.yolo:
        if is_dangerous_command(command):
            return await _handle_dangerous_command(command, agent, timeout)

    return await _execute_command(command, agent, timeout)


_DANGEROUS_CHOICES = ["y", "n", "yolo"]


async def _handle_dangerous_command(
    command: str, agent: WingAgent, timeout: int
) -> str:
    """Handle dangerous command by requesting user confirmation."""
    from wing.event import AskEvent

    question = f"⚠️ Dangerous command detected:\n```bash\n{command}\n```\nProceed?"

    # ask_feedback() 注入 tool_call_id 并 emit AskEvent，注册 feedback waiter，
    # 用户回复经 post(tool_call_id=...) 定向 resolve（并发确认互不干扰）。
    while True:
        try:
            feedback = await agent.ask_feedback(
                AskEvent(
                    session_id=agent.session_id,
                    question=question,
                    choices=_DANGEROUS_CHOICES,
                    required=True,
                ),
                timeout=FEEDBACK_TIMEOUT,
            )
        except asyncio.TimeoutError:
            raise ToolError(
                "⚠️ Feedback timeout. The command was considered dangerous. "
                "Please try a different approach or ask user to respond in time."
            )

        action = _parse_feedback(feedback)
        if action is None:
            # 无效回答 —— 换提示 re-ask（ask_feedback 以同一 tool_call_id 重新注册 waiter）
            question = "⚠️ Please choose one of the options."
            continue

        if action == "yolo":
            # Enable yolo for the rest of this session.
            agent.set_yolo(True)
            return await _execute_command(command, agent, timeout)

        if action == "y":
            return await _execute_command(command, agent, timeout)

        # action == "n"
        raise ToolError("❌ Command rejected by user.")


def _parse_feedback(feedback: str) -> str | None:
    """Parse user feedback for dangerous command.

    Args:
        feedback: User's feedback string.

    Returns:
        One of "y", "n", "yolo", or None if not recognized.
    """
    value = feedback.strip().lower()
    if value in ("y", "n", "yolo"):
        log.info(f"_parse_feedback('{feedback}') -> '{value}'")
        return value
    log.info(f"_parse_feedback('{feedback}') -> None")
    return None


async def _drain_pipes(
    process: asyncio.subprocess.Process, timeout: float = 1.0
) -> tuple[bytes, bytes]:
    """Drain stdout/stderr pipes after process exit, with bounded total wait.

    Background children (`cmd &`) inherit the pipe write-ends, so pipes
    may not reach EOF after the shell exits. The total timeout caps the
    entire drain — not per-read — so a chatty background process cannot
    keep us waiting indefinitely.
    """
    stdout_chunks: list[bytes] = []
    stderr_chunks: list[bytes] = []

    async def _read_stream(
        stream: asyncio.StreamReader | None, chunks: list[bytes]
    ) -> None:
        if stream is None:
            return
        while not stream.at_eof():
            try:
                chunk = await stream.read(65536)
                if not chunk:
                    break
                chunks.append(chunk)
            except OSError:
                break

    try:
        await asyncio.wait_for(
            asyncio.gather(
                _read_stream(process.stdout, stdout_chunks),
                _read_stream(process.stderr, stderr_chunks),
            ),
            timeout=timeout,
        )
    except asyncio.TimeoutError:
        pass  # time's up, return what we have
    return b"".join(stdout_chunks), b"".join(stderr_chunks)


async def _execute_command(command: str, agent: WingAgent, timeout: int) -> str:
    """Execute a shell command. Stateless — no cwd persistence across calls.

    - wait() returns when the shell exits (not when pipe EOF, which
      background children keep open).
    - start_new_session=True isolates the process group so
      os.killpg() can clean up the entire tree on timeout.

    On timeout, partial stdout/stderr is collected and returned so the
    agent can reason about what happened before the kill.
    """
    cwd = agent.state.get("cwd")

    try:
        start_time = time.monotonic()
        process = await asyncio.create_subprocess_shell(
            command,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=cwd if isinstance(cwd, str) else None,
            start_new_session=True,
        )

        agent.state.set("_active_process", process)
        try:
            # Wait for the shell process itself to exit (not children from &).
            # communicate() would block on pipe EOF which background children
            # keep open — wait() returns as soon as the shell exits.
            try:
                await asyncio.wait_for(process.wait(), timeout=timeout)
            except asyncio.TimeoutError:
                # Pre-kill drain: collect whatever output has been produced
                stdout, stderr = await _drain_pipes(process, timeout=0.5)
                kill_process_group(process)
                # Post-kill drain: SIGKILL closes pipe write-ends, remaining
                # buffer becomes readable now
                post_out, post_err = await _drain_pipes(process, timeout=0.3)
                stdout += post_out
                stderr += post_err
                elapsed = int(time.monotonic() - start_time)
                raise ToolError(_format_timeout_result(stdout, stderr, elapsed))

            # Shell exited — drain remaining pipe output with short grace period.
            # Background children may still be writing; we give them 1s then
            # move on. The children continue running detached.
            stdout, stderr = await _drain_pipes(process, timeout=1.0)
        finally:
            agent.state.delete("_active_process")

        rc = process.returncode if process.returncode is not None else -1
        elapsed = int(time.monotonic() - start_time)
        result = _format_result(rc, stdout, stderr, elapsed)
        if rc != 0:
            raise ToolError(result)
        return result

    except ToolError:
        raise
    except Exception as e:
        raise ToolError(f"[exit code: -1]\nError executing command: {e}")


def _format_result(returncode: int, stdout: bytes, stderr: bytes, elapsed: int) -> str:
    stdout_str = stdout.decode("utf-8", errors="replace") if stdout else ""
    stderr_str = stderr.decode("utf-8", errors="replace") if stderr else ""
    parts = []
    if stdout_str:
        parts.append(stdout_str)
    if stderr_str:
        parts.append(f"[stderr] {stderr_str}")
    result = "\n".join(parts) if parts else ""
    return (
        f"[exit code: {returncode} | {elapsed}s]\n{result}"
        if result
        else f"[exit code: {returncode} | {elapsed}s]"
    )


def _format_timeout_result(stdout: bytes, stderr: bytes, elapsed: int) -> str:
    stdout_str = stdout.decode("utf-8", errors="replace") if stdout else ""
    stderr_str = stderr.decode("utf-8", errors="replace") if stderr else ""
    parts = [f"Error: Command timed out after {elapsed}s"]
    if stdout_str:
        parts.append(f"\n--- partial stdout ---\n{stdout_str}")
    if stderr_str:
        parts.append(f"\n--- partial stderr ---\n{stderr_str}")
    return "[exit code: -1]\n" + "\n".join(parts)
