"""Tests for schema serialization — the contract between our Message model and the OpenAI API.

These tests guard against data loss during the to_openai() conversion.
Every field that matters to the LLM must survive the round-trip.
"""

import json

from wing.schema import Message, ToolCall


class TestMessageToOpenai:
    """Verify Message.to_openai() preserves all LLM-facing fields."""

    def test_user_content_preserved(self):
        msg = Message(role="user", content="你好世界")
        o = msg.to_openai()
        assert o["role"] == "user"
        assert o["content"] == "你好世界"

    def test_assistant_content_only(self):
        msg = Message(role="assistant", content="Hello")
        o = msg.to_openai()
        assert o["role"] == "assistant"
        assert o["content"] == "Hello"
        assert "tool_calls" not in o

    def test_assistant_content_with_tool_calls(self):
        """The bug that cost us 2 months: content was overwritten to None when tool_calls existed."""
        msg = Message(
            role="assistant",
            content="Let me search for that.",
            tool_calls=[ToolCall(id="tc_1", name="Bash", arguments={"command": "ls"})],
        )
        o = msg.to_openai()
        assert o["content"] == "Let me search for that.", (
            "content must NOT be null when tool_calls exist"
        )
        assert len(o["tool_calls"]) == 1
        assert o["tool_calls"][0]["function"]["name"] == "Bash"

    def test_assistant_tool_calls_no_content(self):
        """Assistant calls a tool without any text — content should be null."""
        msg = Message(
            role="assistant",
            content=None,
            tool_calls=[ToolCall(id="tc_2", name="Bash", arguments={"command": "pwd"})],
        )
        o = msg.to_openai()
        assert o["content"] is None
        assert len(o["tool_calls"]) == 1

    def test_assistant_no_content_no_tool_calls(self):
        """Edge case: assistant with neither content nor tool_calls.
        content field must still exist (strict providers validate this)."""
        msg = Message(role="assistant", content=None)
        o = msg.to_openai()
        assert o["role"] == "assistant"
        assert "content" in o
        assert o["content"] is None

    def test_assistant_reasoning_with_tool_calls(self):
        """Full real-world scenario: reasoning + content + tool_calls all at once."""
        msg = Message(
            role="assistant",
            reasoning_content="Let me update the plan.",
            content="I found the issue.",
            tool_calls=[ToolCall(id="tc_3", name="TodoWrite", arguments={"todos": []})],
        )
        o = msg.to_openai()
        assert o["content"] == "I found the issue."
        assert o["reasoning_content"] == "Let me update the plan."
        assert len(o["tool_calls"]) == 1

    def test_tool_result_preserved(self):
        msg = Message(
            role="tool",
            tool_call_id="tc_1",
            content="hello world\n[exit code: 0]",
        )
        o = msg.to_openai()
        assert o["role"] == "tool"
        assert o["tool_call_id"] == "tc_1"
        assert o["content"] == "hello world\n[exit code: 0]"

    def test_system_content(self):
        msg = Message(role="system", content="You are a helpful assistant.")
        o = msg.to_openai()
        assert o["content"] == "You are a helpful assistant."

    def test_round_trip_no_data_loss(self):
        """Golden test: to_openai() must never drop fields that affect LLM behavior.

        We serialize to OpenAI format and verify every meaningful field survives.
        This is the single test that would have caught our 2-month bug.
        """
        messages = [
            Message(role="system", content="Be helpful"),
            Message(role="user", content="Analyze this code"),
            Message(
                role="assistant",
                content="I'll look at it.",
                reasoning_content="Thinking about approach...",
                tool_calls=[
                    ToolCall(
                        id="call_abc", name="Bash", arguments={"command": "cat file.py"}
                    )
                ],
            ),
            Message(role="tool", tool_call_id="call_abc", content="file contents here"),
            Message(role="assistant", content="Here's my analysis: the code is fine."),
        ]

        openai_msgs = [m.to_openai() for m in messages]

        # Verify each message's critical fields
        assert openai_msgs[0]["content"] == "Be helpful"
        assert openai_msgs[1]["content"] == "Analyze this code"
        # The critical case — content + tool_calls coexist
        assert openai_msgs[2]["content"] == "I'll look at it."
        assert openai_msgs[2]["reasoning_content"] == "Thinking about approach..."
        assert openai_msgs[2]["tool_calls"][0]["id"] == "call_abc"
        assert openai_msgs[3]["tool_call_id"] == "call_abc"
        assert openai_msgs[3]["content"] == "file contents here"
        assert openai_msgs[4]["content"] == "Here's my analysis: the code is fine."

    def test_tool_call_json_arguments(self):
        """Tool call arguments must serialize as valid JSON string."""
        tc = ToolCall(
            id="tc_1", name="Bash", arguments={"command": "ls", "timeout": 30}
        )
        o = tc.to_openai()
        assert o["function"]["name"] == "Bash"
        # arguments must be a valid JSON string
        args = json.loads(o["function"]["arguments"])
        assert args["command"] == "ls"
        assert args["timeout"] == 30
