"""E2E tests for wing stdio mode — direct subprocess invocation.

These tests run the ``wing`` binary directly via subprocess, verifying
that the three output formats (text / json / stream-json) produce
correctly formatted stdout without going through the SDK layer.

Each test uses a simple prompt (``"say hello"``) that should not trigger
any tool calls — the LLM just replies with text.
"""

import json

import anyio
import pytest


@pytest.mark.anyio
async def test_text_mode(wing_binary_path: str) -> None:
    """``wing -p "say hello"`` should print a non-empty text string to stdout."""
    result = await anyio.run_process(
        [wing_binary_path, "-p", "say hello"],
    )

    assert result.returncode == 0, f"wing exited with code {result.returncode}"
    stdout = result.stdout.decode().strip()
    assert len(stdout) > 0, "text mode should produce non-empty stdout"


@pytest.mark.anyio
async def test_json_mode(wing_binary_path: str) -> None:
    """``wing -p "say hello" --output-format json`` should emit a valid
    result JSON with the expected fields."""
    result = await anyio.run_process(
        [wing_binary_path, "-p", "say hello", "--output-format", "json"],
    )

    assert result.returncode == 0, (
        f"wing exited with code {result.returncode}\n"
        f"stderr: {result.stderr.decode()[:500]}"
    )
    stdout = result.stdout.decode().strip()
    data = json.loads(stdout)

    # ResultMessage schema (Claude-compatible NDJSON).
    assert data["type"] == "result"
    assert data["subtype"] == "success"
    assert data["is_error"] is False
    assert isinstance(data["num_turns"], int)
    assert data["num_turns"] >= 1
    assert isinstance(data["duration_ms"], int)
    assert data["duration_ms"] > 0
    assert isinstance(data["session_id"], str)
    assert len(data["session_id"]) > 0


@pytest.mark.anyio
async def test_stream_json_mode(wing_binary_path: str) -> None:
    """``wing -p "say hello" --output-format stream-json`` should emit
    NDJSON with system/init, assistant, and result message types."""
    result = await anyio.run_process(
        [wing_binary_path, "-p", "say hello", "--output-format", "stream-json"],
    )

    assert result.returncode == 0, (
        f"wing exited with code {result.returncode}\n"
        f"stderr: {result.stderr.decode()[:500]}"
    )
    stdout = result.stdout.decode().strip()
    lines = [ln for ln in stdout.splitlines() if ln.strip()]
    assert len(lines) > 0, "stream-json mode should produce at least one NDJSON line"

    messages = [json.loads(line) for line in lines]
    msg_types = {m["type"] for m in messages}

    # Must contain system, assistant, and result message types.
    assert "system" in msg_types, f"missing 'system' message, got: {msg_types}"
    assert "assistant" in msg_types, f"missing 'assistant' message, got: {msg_types}"
    assert "result" in msg_types, f"missing 'result' message, got: {msg_types}"

    # Verify system/init message structure.
    init_msgs = [m for m in messages if m["type"] == "system"]
    assert len(init_msgs) >= 1
    init = init_msgs[0]
    assert init["subtype"] == "init"
    assert isinstance(init["tools"], list)
    assert len(init["tools"]) > 0
    assert isinstance(init["model"], str)
    assert len(init["model"]) > 0
    assert "session_id" in init

    # Verify assistant message has content blocks.
    assistant_msgs = [m for m in messages if m["type"] == "assistant"]
    assert len(assistant_msgs) >= 1
    content = assistant_msgs[0]["message"]["content"]
    assert isinstance(content, list)
    assert len(content) > 0

    # Verify result message structure.
    result_msgs = [m for m in messages if m["type"] == "result"]
    assert len(result_msgs) == 1
    res = result_msgs[0]
    assert res["subtype"] == "success"
    assert res["is_error"] is False
    assert isinstance(res["num_turns"], int)
    assert isinstance(res["duration_ms"], int)


@pytest.mark.anyio
async def test_unknown_args_ignored(wing_binary_path: str) -> None:
    """Claude-specific flags that wing doesn't recognize should be silently
    dropped rather than causing a clap error.

    Note: wing's arg filter assumes every unknown ``--flag`` (without ``=``)
    consumes the next token as its value. We test only value-carrying unknown
    flags to avoid filter edge cases with unknown boolean flags.
    """
    result = await anyio.run_process(
        [
            wing_binary_path,
            "-p",
            "say hi",
            "--permission-mode",
            "bypassPermissions",
            "--setting-sources",
            "user",
            "--mcp-config",
            "{}",
        ],
    )

    assert result.returncode == 0, (
        f"wing should ignore unknown flags, but exited with {result.returncode}\n"
        f"stderr: {result.stderr.decode()[:500]}"
    )
    stdout = result.stdout.decode().strip()
    assert len(stdout) > 0


@pytest.mark.anyio
async def test_verbose_flag_ignored(wing_binary_path: str) -> None:
    """``--verbose`` is a boolean flag sent by the SDK that wing must
    silently accept without consuming the next token as its value."""
    result = await anyio.run_process(
        [
            wing_binary_path,
            "-p",
            "say hi",
            "--verbose",
            "--system-prompt",
            "",
        ],
    )

    assert result.returncode == 0, (
        f"wing should accept --verbose without error, exited with {result.returncode}\n"
        f"stderr: {result.stderr.decode()[:500]}"
    )

    assert result.returncode == 0, (
        f"wing should ignore unknown flags, but exited with {result.returncode}\n"
        f"stderr: {result.stderr.decode()[:500]}"
    )
    stdout = result.stdout.decode().strip()
    assert len(stdout) > 0
