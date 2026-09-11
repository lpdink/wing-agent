"""Tests for schema serialization — the contract between our Message model and the OpenAI API.

These tests guard against data loss during the to_openai() conversion.
Every field that matters to the LLM must survive the round-trip.
"""

import json

from wing.schema import Message, ThinkingBlock, ToolCall


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
        """Assistant calls a tool without any text — content is empty string, not null.

        Wire 契约：content 恒为非 null 字符串（严格 OpenAI 兼容网关对 null
        直接 400，即使带 tool_calls 的轮次也不必冒险）。
        """
        msg = Message(
            role="assistant",
            content=None,
            tool_calls=[ToolCall(id="tc_2", name="Bash", arguments={"command": "pwd"})],
        )
        o = msg.to_openai()
        assert o["content"] == ""
        assert len(o["tool_calls"]) == 1

    def test_assistant_no_content_no_tool_calls(self):
        """Edge case: assistant with neither content nor tool_calls (存量零块
        ghost 记录的形态)。content 字段必须存在且为非 null 字符串。"""
        msg = Message(role="assistant", content=None)
        o = msg.to_openai()
        assert o["role"] == "assistant"
        assert o["content"] == ""

    def test_assistant_thinking_only_content_is_empty_string(self):
        """打断补提交 / max_tokens 截断在 thinking 中途：仅含 thinking 的
        assistant 消息 → content 为非 null 空串 + reasoning_content 原样保留。

        回归（#68）：此类消息此前序列化为 content:null，阿里云 MaaS 等严格
        网关对「无 tool_calls 且 content 缺失」返回 400，且毒消息持久化后
        污染整个会话。
        """
        msg = Message(
            role="assistant",
            content_blocks=[ThinkingBlock(thinking="half-done reasoning")],
            stop_reason="interrupted",
        )
        o = msg.to_openai()
        assert o["content"] == ""
        assert o["reasoning_content"] == "half-done reasoning"

        # 经过 JSON 序列化后仍是字符串（回归断言：不得出现 "content": null）
        assert '"content": null' not in json.dumps(o, ensure_ascii=False)

    def test_assistant_max_tokens_truncated_at_thinking(self):
        """max_tokens 截断在 thinking 中途：同一序列化路径，content 非 null。"""
        msg = Message(
            role="assistant",
            content_blocks=[ThinkingBlock(thinking="truncated thought")],
            stop_reason="max_tokens",
        )
        o = msg.to_openai()
        assert o["content"] == ""
        assert o["reasoning_content"] == "truncated thought"

    def test_legacy_flat_thinking_only_record(self):
        """存量旧格式记录（扁平 reasoning_content、无块数组）加载后同样不回 null。"""
        msg = Message.model_validate(
            {
                "role": "assistant",
                "reasoning_content": "old reasoning",
                "stop_reason": "interrupted",
            }
        )
        o = msg.to_openai()
        assert o["content"] == ""
        assert o["reasoning_content"] == "old reasoning"

    def test_tool_message_empty_content_is_empty_string(self):
        """tool 消息空结果：严格网关同样要求 content 存在且非 null。"""
        msg = Message(role="tool", tool_call_id="tc_1", content="")
        o = msg.to_openai()
        assert o["content"] == ""

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


class TestToolToOpenai:
    """Verify Tool.to_openai() uses effective_llm_name."""

    def test_default_llm_name_uses_name(self):
        from wing.schema import Tool

        tool = Tool(
            name="Bash", description="run shell", params=[], function=lambda: None
        )
        schema = tool.to_openai()
        assert schema["function"]["name"] == "Bash"

    def test_explicit_llm_name_overrides(self):
        from wing.schema import Tool

        tool = Tool(
            name="Bash",
            llm_name="Shell",
            description="run shell",
            params=[],
            function=lambda: None,
        )
        schema = tool.to_openai()
        assert schema["function"]["name"] == "Shell"

    def test_effective_llm_name_property(self):
        from wing.schema import Tool

        t1 = Tool(name="Bash", description="", params=[], function=lambda: None)
        assert t1.effective_llm_name == "Bash"

        t2 = Tool(
            name="Bash",
            llm_name="Shell",
            description="",
            params=[],
            function=lambda: None,
        )
        assert t2.effective_llm_name == "Shell"
