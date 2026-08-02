"""Message 单一存储测试——assistant 消息只存 content_blocks，扁平字段实时派生。

锁定 PR #59 第二轮 review 的核心决策：消灭双存储与手动同步
（sync_flat_from_blocks），content / reasoning_content / tool_calls 为
实时派生访问器；存量旧格式 JSONL 干净加载；序列化导出向后兼容。
"""

import pytest

from wing.schema import (
    ContentBlock,
    Message,
    TextBlock,
    ThinkingBlock,
    ToolCall,
    ToolUseBlock,
)


def _assistant_msg() -> Message:
    return Message(
        role="assistant",
        content_blocks=[
            ThinkingBlock(thinking="let me think", signature="sig1"),
            TextBlock(text="hello "),
            TextBlock(text="world"),
            ThinkingBlock(thinking="encrypted", signature="payload", redacted=True),
            ToolUseBlock(id="t1", name="bash", input={"command": "ls"}),
        ],
    )


class TestDerivedAccessors:
    def test_content_joins_text_blocks(self):
        assert _assistant_msg().content == "hello world"

    def test_reasoning_content_joins_non_redacted_thinking(self):
        # redacted 块的 payload 不是推理文本，不进派生串
        assert _assistant_msg().reasoning_content == "let me think"

    def test_tool_calls_extracted_from_blocks(self):
        tcs = _assistant_msg().tool_calls
        assert tcs is not None and len(tcs) == 1
        assert tcs[0].name == "bash" and tcs[0].arguments == {"command": "ls"}

    def test_empty_blocks_derive_none(self):
        m = Message(role="assistant")
        assert m.content is None
        assert m.reasoning_content is None
        assert m.tool_calls is None

    def test_derivation_is_live_after_block_mutation(self):
        """修改块数组后派生字段实时一致——无需任何同步调用。"""
        m = _assistant_msg()
        blocks = m.content_blocks
        assert blocks is not None
        remaining: list[ContentBlock] = [
            b for b in blocks if not isinstance(b, ThinkingBlock)
        ]
        m.content_blocks = remaining
        assert m.reasoning_content is None
        assert m.content == "hello world"
        assert len(m.tool_calls or []) == 1

    def test_assistant_flat_setters_rejected(self):
        m = _assistant_msg()
        with pytest.raises(AttributeError):
            m.content = "x"
        with pytest.raises(AttributeError):
            m.tool_calls = []


class TestNonAssistantStorage:
    def test_user_message_stores_content(self):
        m = Message(role="user", content="question")
        assert m.content == "question"
        assert m.content_blocks is None

    def test_user_content_setter_writes_storage(self):
        m = Message(role="user", content="a")
        m.content = "b"
        assert m.content == "b"

    def test_tool_message_round_trip(self):
        m = Message(role="tool", tool_call_id="t1", content="result")
        m.content = "result2"
        assert Message.model_validate(m.model_dump()).content == "result2"


class TestAssistantConstruction:
    def test_flat_args_mapped_to_blocks(self):
        """assistant 以扁平入参构造（无 blocks）→ 映射为块存储。"""
        m = Message(
            role="assistant",
            content="hi",
            reasoning_content="hmm",
            tool_calls=[ToolCall(id="x", name="n", arguments={"a": 1})],
        )
        assert m.content_blocks is not None and len(m.content_blocks) == 3
        assert isinstance(m.content_blocks[0], ThinkingBlock)
        assert m.content_blocks[0].signature is None
        assert isinstance(m.content_blocks[1], TextBlock)
        assert isinstance(m.content_blocks[2], ToolUseBlock)
        # 派生值与入参一致
        assert m.content == "hi"
        assert m.reasoning_content == "hmm"
        assert m.tool_calls is not None and m.tool_calls[0].name == "n"

    def test_blocks_win_over_flat_args(self):
        """同时给 blocks 与扁平入参 → 扁平忽略（派生自 blocks）。"""
        m = Message(
            role="assistant",
            content_blocks=[TextBlock(text="real")],
            content="ignored",
        )
        assert m.content == "real"


class TestLegacyCompat:
    def test_legacy_jsonl_loads_clean(self):
        """develop 基线 JSONL（仅扁平字段）干净加载为块存储。"""
        record = {
            "role": "assistant",
            "content": "hi",
            "reasoning_content": "hmm",
            "tool_calls": [{"id": "x", "name": "n", "arguments": {"a": 1}}],
            "uuid": "u1",
            "parent_uuid": None,
        }
        m = Message.model_validate(record)
        assert m.content_blocks is not None and len(m.content_blocks) == 3
        assert m.content == "hi"
        assert m.reasoning_content == "hmm"
        assert m.tool_calls is not None and m.tool_calls[0].arguments == {"a": 1}

    def test_export_backward_compatible(self):
        """导出含派生扁平字段——旧格式读者可正常消费。"""
        m = _assistant_msg()
        d = m.model_dump()
        assert d["content"] == "hello world"
        assert d["reasoning_content"] == "let me think"
        assert d["tool_calls"][0]["name"] == "bash"
        assert d["content_blocks"] is not None

    def test_dump_validate_round_trip_stable(self):
        record = {
            "role": "assistant",
            "content": "hi",
            "reasoning_content": "hmm",
            "tool_calls": [{"id": "x", "name": "n", "arguments": {}}],
        }
        d1 = Message.model_validate(record).model_dump()
        d2 = Message.model_validate(d1).model_dump()
        assert d1 == d2

    def test_legacy_user_message_round_trip(self):
        record = {"role": "user", "content": "hello", "uuid": "u2"}
        m = Message.model_validate(record)
        assert m.content == "hello"
        assert Message.model_validate(m.model_dump()).content == "hello"
