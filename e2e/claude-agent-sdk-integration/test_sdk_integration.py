"""E2E tests for wing stdio mode driven by ``claude-agent-sdk-python``.

These tests use the SDK's ``query()`` function to spawn ``wing`` as a
subprocess, perform the stdin initialize handshake, send a prompt, and
deserialize the NDJSON stdout stream into typed ``Message`` objects.

Assertions verify that wing's NDJSON output is correctly parsed by the
SDK's ``parse_message()`` into the expected dataclass instances with
populated fields — proving protocol compatibility end-to-end.
"""

from collections.abc import Callable

import pytest

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ResultMessage,
    SystemMessage,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
    query,
)


async def _collect_messages(
    prompt: str,
    options: ClaudeAgentOptions,
) -> list:
    """Run ``query()`` and collect all messages into a list."""
    messages: list = []
    async for msg in query(prompt=prompt, options=options):
        messages.append(msg)
    return messages


@pytest.mark.anyio
async def test_simple_prompt(
    sdk_options_factory: Callable[..., ClaudeAgentOptions],
) -> None:
    """A simple text prompt should produce a correctly typed message stream.

    Verifies:
        - ``SystemMessage(subtype="init")`` with tools/model/session_id
        - ``AssistantMessage`` with ``TextBlock`` content and populated model
        - ``ResultMessage`` as the terminal message with correct fields
    """
    options = sdk_options_factory(max_turns=3)
    messages = await _collect_messages(
        "What is 2+2? Reply with just the number.",
        options,
    )

    assert len(messages) > 0, "should receive at least one message"

    # ── SystemMessage(subtype="init") ──────────────────────────────
    init_msgs = [m for m in messages if isinstance(m, SystemMessage)]
    assert len(init_msgs) >= 1, (
        f"expected SystemMessage(subtype='init'), got types: "
        f"{[type(m).__name__ for m in messages]}"
    )
    init = init_msgs[0]
    assert init.subtype == "init"
    assert "tools" in init.data
    assert isinstance(init.data["tools"], list)
    assert len(init.data["tools"]) > 0
    assert "model" in init.data
    assert isinstance(init.data["model"], str)
    assert len(init.data["model"]) > 0
    assert "session_id" in init.data

    # ── AssistantMessage with TextBlock ────────────────────────────
    assistant_msgs = [m for m in messages if isinstance(m, AssistantMessage)]
    assert len(assistant_msgs) >= 1, (
        f"expected at least one AssistantMessage, got types: "
        f"{[type(m).__name__ for m in messages]}"
    )
    # model must be populated
    last_assistant = assistant_msgs[-1]
    assert last_assistant.model != "", "AssistantMessage.model should not be empty"
    # must contain at least one TextBlock
    text_blocks = [b for b in last_assistant.content if isinstance(b, TextBlock)]
    assert len(text_blocks) >= 1, (
        f"expected TextBlock in AssistantMessage.content, got block types: "
        f"{[type(b).__name__ for b in last_assistant.content]}"
    )
    assert len(text_blocks[0].text) > 0

    # ── ResultMessage (terminal) ───────────────────────────────────
    result_msgs = [m for m in messages if isinstance(m, ResultMessage)]
    assert len(result_msgs) == 1, (
        f"expected exactly one ResultMessage, got {len(result_msgs)}"
    )
    result = result_msgs[0]
    assert result.subtype == "success"
    assert result.is_error is False
    assert result.num_turns >= 1
    assert result.duration_ms > 0
    assert isinstance(result.session_id, str)
    assert len(result.session_id) > 0


@pytest.mark.anyio
async def test_bash_tool_call(
    sdk_options_factory: Callable[..., ClaudeAgentOptions],
) -> None:
    """A prompt requesting a Bash command should trigger a tool call.

    Verifies the full tool-call message cycle:
        - ``AssistantMessage`` containing ``ToolUseBlock(name="Bash")``
        - ``UserMessage`` containing ``ToolResultBlock`` with matching ``tool_use_id``
        - Tool result content includes the command's output
        - ``ResultMessage`` as the terminal message
    """
    options = sdk_options_factory(max_turns=5)
    messages = await _collect_messages(
        "Run the command `echo wing_e2e_test_marker` in bash and show me the output.",
        options,
    )

    assert len(messages) > 0, "should receive at least one message"

    # ── AssistantMessage with ToolUseBlock(name="Bash") ────────────
    assistant_msgs = [m for m in messages if isinstance(m, AssistantMessage)]
    assert len(assistant_msgs) >= 1, "expected at least one AssistantMessage"

    all_tool_uses = [
        block
        for msg in assistant_msgs
        for block in msg.content
        if isinstance(block, ToolUseBlock)
    ]
    bash_calls = [b for b in all_tool_uses if b.name == "Bash"]
    assert len(bash_calls) >= 1, (
        f"expected a Bash ToolUseBlock, got tool names: "
        f"{[b.name for b in all_tool_uses]}"
    )

    # Bash input must be a dict with a "command" field containing "echo"
    bash_input = bash_calls[0].input
    assert isinstance(bash_input, dict), (
        f"ToolUseBlock.input should be dict, got {type(bash_input).__name__}"
    )
    assert "command" in bash_input, (
        f"Bash ToolUseBlock.input should have 'command' key, got keys: "
        f"{list(bash_input.keys())}"
    )
    assert "echo" in bash_input["command"], (
        f"command should contain 'echo', got: {bash_input['command']}"
    )

    # ── UserMessage with ToolResultBlock ───────────────────────────
    user_msgs = [m for m in messages if isinstance(m, UserMessage)]
    assert len(user_msgs) >= 1, "expected at least one UserMessage (tool result)"

    all_tool_results = [
        block
        for msg in user_msgs
        if isinstance(msg.content, list)
        for block in msg.content
        if isinstance(block, ToolResultBlock)
    ]
    assert len(all_tool_results) >= 1, (
        f"expected ToolResultBlock in UserMessage, got user msg contents: "
        f"{[type(b).__name__ for msg in user_msgs for b in (msg.content if isinstance(msg.content, list) else [])]}"
    )

    # tool_use_id must match the Bash call's id
    bash_call_ids = {b.id for b in bash_calls}
    result_ids = {b.tool_use_id for b in all_tool_results}
    matched = bash_call_ids & result_ids
    assert matched, (
        f"ToolResultBlock.tool_use_id should match ToolUseBlock.id. "
        f"Bash call ids: {bash_call_ids}, result ids: {result_ids}"
    )

    # Tool result should contain the echo output
    matching_results = [b for b in all_tool_results if b.tool_use_id in bash_call_ids]
    combined_content = " ".join(str(b.content) for b in matching_results)
    assert "wing_e2e_test_marker" in combined_content, (
        f"tool result should contain echo output 'wing_e2e_test_marker', "
        f"got: {combined_content[:500]}"
    )

    # ── Final AssistantMessage should contain text ─────────────────
    final_assistant = assistant_msgs[-1]
    final_text_blocks = [
        b for b in final_assistant.content if isinstance(b, TextBlock)
    ]
    assert len(final_text_blocks) >= 1, (
        f"final AssistantMessage should have TextBlock, got: "
        f"{[type(b).__name__ for b in final_assistant.content]}"
    )
    assert len(final_text_blocks[0].text) > 0

    # ── ResultMessage ──────────────────────────────────────────────
    result_msgs = [m for m in messages if isinstance(m, ResultMessage)]
    assert len(result_msgs) == 1, (
        f"expected exactly one ResultMessage, got {len(result_msgs)}"
    )
    result = result_msgs[0]
    assert result.subtype == "success"
    assert result.is_error is False
    assert result.num_turns >= 1
