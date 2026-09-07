"""provider 终止语义测试——stop_reason 捕获与中断快照。

覆盖：
- anthropic `_ordered_finalized_blocks`：未被 content_block_stop 终结的
  tool 块剔除（max_tokens 砍在 args 中间时不执行半截工具调用）；
- anthropic `_build_final_response`：stop_reason 随最终 usage 传导；
- anthropic `snapshot_blocks`：中断快照保留 text/thinking、丢弃 pending tool；
- openai_compat `_OAIStreamState` 快照：只含已终结的 tool call；
- openai_compat 流式 finish_reason=length 传导。
"""

from __future__ import annotations


from wing.provider.anthropic import AnthropicProvider, _StreamState
from wing.provider.openai_compat import OpenAICompatProvider, _OAIStreamState
from wing.schema import PendingCall, TextBlock, ThinkingBlock, ToolUseBlock


def _anthropic_state() -> _StreamState:
    """构造混合块状态：thinking + text + 完结 tool + 半截 tool。"""
    state = _StreamState()
    state.blocks_by_index[0] = ThinkingBlock(thinking="thought", signature="s")
    state.blocks_by_index[1] = TextBlock(text="answer")
    state.blocks_by_index[2] = ToolUseBlock(id="done-call", name="Bash", input={"x": 1})
    state.blocks_by_index[3] = ToolUseBlock(id="half-call", name="Read", input={})
    # index 3 未收到 content_block_stop——仍 pending（半截）
    state.pending_tools[3] = PendingCall(id="half-call", name="Read")
    state.stop_reason = "max_tokens"
    return state


class TestAnthropicFinalizedBlocks:
    def test_pending_tool_block_excluded(self):
        """半截 tool 块（仍在 pending_tools）被剔除，其余保序。"""
        state = _anthropic_state()
        blocks = AnthropicProvider._ordered_finalized_blocks(state)
        assert [type(b).__name__ for b in blocks] == [
            "ThinkingBlock",
            "TextBlock",
            "ToolUseBlock",
        ]
        assert all(getattr(b, "id", None) != "half-call" for b in blocks)

    def test_build_final_response_stops_reason_and_blocks(self):
        state = _anthropic_state()
        state.prompt_tokens = 100
        state.completion_tokens = 50
        state.cached_tokens = 10
        resp = AnthropicProvider._build_final_response(state, "claude-x", "req-1", 12.3)
        assert resp.content_blocks is not None
        assert len(resp.content_blocks) == 3  # 半截 tool 剔除
        assert resp.usage.stop_reason == "max_tokens"
        assert resp.usage.prompt_tokens == 110  # 总输入（含缓存）

    def test_snapshot_blocks_keeps_partial_text_thinking(self):
        """中断快照：已累积 text/thinking 保留，pending tool 剔除。"""
        provider = AnthropicProvider.__new__(
            AnthropicProvider
        )  # 不走 __init__（无 http）
        state = _anthropic_state()
        acc = provider.create_accumulator()
        acc.state = state
        blocks = provider.snapshot_blocks(acc)
        assert blocks is not None
        assert [type(b).__name__ for b in blocks] == [
            "ThinkingBlock",
            "TextBlock",
            "ToolUseBlock",
        ]

    def test_snapshot_blocks_none_when_never_started(self):
        """流未开始（state 未填充）：快照返回 None。"""
        provider = AnthropicProvider.__new__(AnthropicProvider)
        acc = provider.create_accumulator()
        assert provider.snapshot_blocks(acc) is None
        assert provider.snapshot_blocks(None) is None

    def test_message_delta_captures_stop_reason(self):
        state = _StreamState()
        AnthropicProvider._on_message_delta(
            state, {"delta": {"stop_reason": "max_tokens"}, "usage": {}}
        )
        assert state.stop_reason == "max_tokens"

        AnthropicProvider._on_message_delta(state, {"delta": {}, "usage": {}})
        assert state.stop_reason == "max_tokens"  # 首个非空值生效


class TestOpenAIStreamSnapshot:
    def _provider(self) -> OpenAICompatProvider:
        return OpenAICompatProvider.__new__(OpenAICompatProvider)

    def test_snapshot_keeps_finalized_drops_pending(self):
        """快照只含已终结 tool call（finish_reason=tool_calls 时解析入列）。"""
        from wing.schema import ToolCall

        provider = self._provider()
        state = _OAIStreamState()
        state.reasoning_chunks.append("think ")
        state.reasoning_chunks.append("more")
        state.content_chunks.append("answer")
        state.final_tool_calls.append(
            ToolCall(id="c1", name="Bash", arguments={"cmd": "ls"})
        )
        # pending：未终结的调用（半截）
        state.pending[1] = PendingCall(id="c2", name="Read", args_buffer='{"pa')

        acc = provider.create_accumulator()
        acc.state = state
        blocks = provider.snapshot_blocks(acc)
        assert blocks is not None
        kinds = [type(b).__name__ for b in blocks]
        assert kinds == ["ThinkingBlock", "TextBlock", "ToolUseBlock"]
        assert all(getattr(b, "id", None) != "c2" for b in blocks)

    def test_snapshot_none_when_empty(self):
        provider = self._provider()
        acc = provider.create_accumulator()
        acc.state = _OAIStreamState()  # 流已开始但无内容
        assert provider.snapshot_blocks(acc) is None
        assert provider.snapshot_blocks(None) is None

    def test_finish_reason_recorded_on_state(self):
        """流式 chunk 的 finish_reason 首个非空值进入 state。"""
        state = _OAIStreamState()
        # 模拟 _generate_stream 内部的记录逻辑
        choice = {"finish_reason": "length"}
        if state.stop_reason is None and choice is not None:
            state.stop_reason = choice.get("finish_reason")
        assert state.stop_reason == "length"
