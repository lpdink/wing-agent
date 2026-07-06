import asyncio


async def execute_shell(command: str, timeout: int = 30) -> str:
    """Execute a shell command in a specific directory.

    Args:
        command: The command to execute.
        timeout: Maximum wait time in seconds.

    Returns:
        Command output with exit code.
    """

    try:
        process = await asyncio.create_subprocess_shell(
            command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        try:
            stdout, stderr = await asyncio.wait_for(
                process.communicate(), timeout=timeout
            )
        except asyncio.TimeoutError:
            process.kill()
            return f"[exit code: -1]\nError: Command timed out after {timeout} seconds"

        output = []
        if stdout:
            output.append(stdout.decode("utf-8", errors="replace"))
        if stderr:
            output.append(f"[stderr] {stderr.decode('utf-8', errors='replace')}")

        return (
            f"[exit code: {process.returncode}]\n" + "\n".join(output)
            if output
            else f"[exit code: {process.returncode}]"
        )

    except Exception as e:
        return f"[exit code: -1]\nError executing command: {str(e)}"
