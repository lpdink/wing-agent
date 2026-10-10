"""provider 终止语义测试——stop_reason 捕获与中断快照。

覆盖：
- anthropic `_ordered_finalized_blocks`：未被 content_block_stop 终结的
  tool 块剔除（max_tokens 砍在 args 中间时不执行半截工具调用）；
- anthropic `_build_final_response`：stop_reason 随最终 usage 传导；
- anthropic `snapshot_blocks`：中断快照保留 text/thinking、丢弃 pending tool；
- openai_compat `_OAIStreamState` 快照：只含已终结的 tool call；
- openai_compat 流式 finish_reason=length 传导：真实 `_generate_stream` 上，
  带内 usage 帧与流尾终帧都携带 stop_reason（帧序两种都覆盖）。
"""

from __future__ import annotations

import json

import pytest

from wing.provider.anthropic.provider import AnthropicProvider
from wing.provider.anthropic.stream import _StreamState
from wing.provider.openai.provider import OpenAICompatProvider
from wing.provider.openai.stream import _OAIStreamState
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


class TestUnfinishedToolCallCount:
    """截断检测计数：含无 id 的半截调用（投影会跳过它们，计数不能有盲区）。"""

    def test_anthropic_counts_id_less_pending(self):
        provider = AnthropicProvider.__new__(AnthropicProvider)
        state = _StreamState()
        state.pending_tools[0] = PendingCall(args_buffer='{"pa')  # 无 id
        acc = provider.create_accumulator()
        acc.state = state
        assert provider.unfinished_tool_calls(acc) == 1
        assert provider.pending_tool_calls(acc) == []  # 投影仍跳过无 id（无法锚定）

    def test_openai_counts_id_less_pending(self):
        provider = OpenAICompatProvider.__new__(OpenAICompatProvider)
        state = _OAIStreamState()
        state.pending[0] = PendingCall(args_buffer='{"pa')  # 无 id
        acc = provider.create_accumulator()
        acc.state = state
        assert provider.unfinished_tool_calls(acc) == 1
        assert provider.pending_tool_calls(acc) == []

    def test_zero_when_never_started(self):
        provider = OpenAICompatProvider.__new__(OpenAICompatProvider)
        assert provider.unfinished_tool_calls(None) == 0
        acc = provider.create_accumulator()
        assert provider.unfinished_tool_calls(acc) == 0


# ─── OpenAI 兼容流式：stop_reason 的帧级传导（#151）─────────────────


class _FakeStreamResponse:
    is_error = False

    def __init__(self, lines: list[str]) -> None:
        self._lines = lines
        self.headers = {"x-request-id": "rid-stop-reason"}

    async def aiter_lines(self):
        for line in self._lines:
            yield line

    async def aclose(self) -> None:
        pass


class _FakeStreamClient:
    def __init__(self, lines: list[str]) -> None:
        self._lines = lines

    def build_request(self, *args: object, **kwargs: object) -> object:
        return object()

    async def send(self, request: object, stream: bool = False) -> _FakeStreamResponse:
        return _FakeStreamResponse(self._lines)


def _openai_sse(chunks: list[dict]) -> list[str]:
    lines: list[str] = []
    for chunk in chunks:
        lines.append(f"data: {json.dumps(chunk)}")
        lines.append("")
    lines.append("data: [DONE]")
    lines.append("")
    return lines


def _openai_stream_provider(chunks: list[dict]) -> OpenAICompatProvider:
    """只装配 `_generate_stream` 需要的属性（不走 __init__：无 config / http）。"""
    provider = OpenAICompatProvider.__new__(OpenAICompatProvider)
    provider.timeout_first_chunk = 30.0
    provider._client = _FakeStreamClient(_openai_sse(chunks))
    return provider


def _usage_frames(responses: list) -> tuple[list, list]:
    """(带内非零 usage 帧, 权威块数组终帧)。"""
    inband = [
        r for r in responses if r.usage.completion_tokens or r.usage.prompt_tokens
    ]
    final = [r for r in responses if r.content_blocks is not None]
    return inband, final


class TestOpenAIStreamStopReason:
    """流式 finish_reason=length：带内 usage 帧与终帧都携带 stop_reason。

    带内帧是 llm_call_metrics 的载荷源（直播截断提示），终帧是
    Message.stop_reason 的取值源（落盘审计）；任一缺失都会让"length ≠ stop"
    在对应路径上不可见。
    """

    @pytest.mark.asyncio
    async def test_usage_frame_after_finish_reason_carries_it(self):
        """协议帧序（finish_reason 先、usage 后）：带内 usage 帧带 stop_reason。"""
        chunks = [
            {"choices": [{"delta": {"content": "cut o"}}]},
            {"choices": [{"delta": {}, "finish_reason": "length"}]},
            {"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
        ]
        provider = _openai_stream_provider(chunks)
        acc = provider.create_accumulator()

        responses = [
            c async for c in provider._generate_stream({}, "gpt-4", accumulator=acc)
        ]

        inband, final = _usage_frames(responses)
        assert len(inband) == 1, [r.model_dump() for r in responses]
        assert inband[0].usage.stop_reason == "length"
        assert len(final) == 1
        assert final[0].usage.stop_reason == "length"

    @pytest.mark.asyncio
    async def test_usage_frame_before_finish_reason_falls_back_to_final(self):
        """帧序反了（usage 先到）：带内帧给不出值，终帧兜底（取值侧按帧全量捕获）。"""
        chunks = [
            {"choices": [{"delta": {"content": "cut o"}}]},
            {"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
            {"choices": [{"delta": {}, "finish_reason": "length"}]},
        ]
        provider = _openai_stream_provider(chunks)
        acc = provider.create_accumulator()

        responses = [
            c async for c in provider._generate_stream({}, "gpt-4", accumulator=acc)
        ]

        inband, final = _usage_frames(responses)
        assert len(inband) == 1
        assert inband[0].usage.stop_reason is None  # 此刻上游还没给
        assert len(final) == 1
        assert final[0].usage.stop_reason == "length"
