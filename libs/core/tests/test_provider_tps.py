# tests/test_provider_tps.py
"""decode TPS 回归——纯 tool call 响应必须带速度。

背景：首 token 打点（``first_token_ts``）只覆盖正文 / 思考增量时，模型"只回
tool call"（无 text、无 thinking）的轮次 ``tokens_per_sec`` 恒为 0；前端按
``> 0`` 决定是否展示，用户只看到 in/out/ttft 而看不到 TPS。修复：tool 参数
增量（OpenAI ``tool_calls`` 增量 / Anthropic ``input_json_delta``）同样参与
打点——它们也是模型解码产出。

锁定：
- OpenAI：tool-call-only 流（含带内 usage 末帧）→ 非零 usage 的
  ``tokens_per_sec`` 为正；
- Anthropic：纯 tool_use 流（input_json_delta）→ ``message_stop`` 的
  ``tokens_per_sec`` 为正；
- 对照组：正文流语义不回归。
"""

from __future__ import annotations

import json

import pytest

from wing.config import ProviderConfig
from wing.provider.anthropic import AnthropicProvider, _StreamState
from wing.provider.openai_compat import OpenAICompatProvider, _OAIStreamState
from wing.schema import ToolUseBlock

# ─── SSE 伪造 ────────────────────────────────────────────────────


class _FakeResponse:
    is_error = False

    def __init__(self, lines: list[str]) -> None:
        self._lines = lines
        self.headers = {"request-id": "tps-rid", "x-request-id": "tps-rid"}

    async def aiter_lines(self):
        for line in self._lines:
            yield line

    async def aclose(self) -> None:
        pass


class _FakeClient:
    def __init__(self, lines: list[str]) -> None:
        self._lines = lines

    def build_request(self, *args: object, **kwargs: object) -> object:
        return object()

    async def send(self, request: object, stream: bool = False) -> _FakeResponse:
        return _FakeResponse(self._lines)


def _openai_sse(chunks: list[dict]) -> list[str]:
    lines: list[str] = []
    for c in chunks:
        lines.append(f"data: {json.dumps(c)}")
        lines.append("")
    lines.append("data: [DONE]")
    lines.append("")
    return lines


def _anthropic_sse(events: list[dict]) -> list[str]:
    lines: list[str] = []
    for ev in events:
        lines.append(f"event: {ev.get('type', 'unknown')}")
        lines.append(f"data: {json.dumps(ev)}")
        lines.append("")
    return lines


async def _replace_client(
    provider: OpenAICompatProvider | AnthropicProvider, lines: list[str]
) -> None:
    """关闭真实 httpx client，替换为伪造流。"""
    await provider._client.aclose()
    provider._client = _FakeClient(lines)  # ty: ignore[invalid-assignment]


def _openai_provider() -> OpenAICompatProvider:
    return OpenAICompatProvider(
        ProviderConfig(
            name="test-openai-tps",
            protocol="openai",
            base_url="https://api.example.com",
            api_key="sk-test",
        )
    )


def _anthropic_provider() -> AnthropicProvider:
    return AnthropicProvider(
        ProviderConfig(
            name="test-anthropic-tps",
            protocol="anthropic",
            base_url="https://api.anthropic.com",
            api_key="sk-test",
        )
    )


# ─── OpenAI-compat ──────────────────────────────────────────────


class TestOpenAIDecodeTps:
    @pytest.mark.asyncio
    async def test_tool_call_only_stream_sets_decode_tps(self):
        """纯 tool call 流（无 content / reasoning）：非零 usage 的 TPS 为正。"""
        chunks = [
            {"choices": [{"delta": {"role": "assistant", "content": ""}}]},
            {
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "c1",
                                    "type": "function",
                                    "function": {"name": "Bash", "arguments": ""},
                                }
                            ]
                        }
                    }
                ]
            },
            {
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "function": {"arguments": '{"cmd": "ls"}'},
                                }
                            ]
                        }
                    }
                ]
            },
            {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
            {
                "choices": [],
                "usage": {"prompt_tokens": 10, "completion_tokens": 12},
            },
        ]
        p = _openai_provider()
        await _replace_client(p, _openai_sse(chunks))
        acc = p.create_accumulator()

        usage_tps: float | None = None
        blocks = None
        async for chunk in p._generate_stream({}, "gpt-4", accumulator=acc):
            if chunk.usage.completion_tokens:
                usage_tps = chunk.usage.tokens_per_sec
            if chunk.content_blocks is not None:
                blocks = chunk.content_blocks

        state = acc.state
        assert isinstance(state, _OAIStreamState)
        assert state.first_token_ts is not None, "tool 增量必须打点首 token"
        assert usage_tps is not None
        assert usage_tps > 0, usage_tps
        assert blocks is not None
        assert len(blocks) == 1
        assert isinstance(blocks[0], ToolUseBlock)
        assert blocks[0].input == {"cmd": "ls"}

    @pytest.mark.asyncio
    async def test_text_stream_still_sets_decode_tps(self):
        """对照组：正文流的 TPS 语义不回归。"""
        chunks = [
            {"choices": [{"delta": {"content": "hello"}}]},
            {"choices": [{"delta": {}, "finish_reason": "stop"}]},
            {"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3}},
        ]
        p = _openai_provider()
        await _replace_client(p, _openai_sse(chunks))
        acc = p.create_accumulator()

        usage_tps: float | None = None
        async for chunk in p._generate_stream({}, "gpt-4", accumulator=acc):
            if chunk.usage.completion_tokens:
                usage_tps = chunk.usage.tokens_per_sec

        state = acc.state
        assert isinstance(state, _OAIStreamState)
        assert state.first_token_ts is not None
        assert usage_tps is not None
        assert usage_tps > 0, usage_tps


# ─── Anthropic ──────────────────────────────────────────────────


class TestAnthropicDecodeTps:
    @pytest.mark.asyncio
    async def test_tool_use_only_stream_sets_decode_tps(self):
        """纯 tool_use 流（无 text / thinking）：message_stop 的 TPS 为正。"""
        events = [
            {"type": "message_start", "message": {"usage": {"input_tokens": 10}}},
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Bash"},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": '{"cmd":'},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": '"ls"}'},
            },
            {"type": "content_block_stop", "index": 0},
            {
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 12},
            },
            {"type": "message_stop"},
        ]
        p = _anthropic_provider()
        await _replace_client(p, _anthropic_sse(events))
        acc = p.create_accumulator()

        final = None
        async for chunk in p._generate_stream({}, "claude-x", accumulator=acc):
            if chunk.content_blocks is not None:
                final = chunk

        state = acc.state
        assert isinstance(state, _StreamState)
        assert state.first_token_ts is not None, "input_json_delta 必须打点首 token"
        assert final is not None
        assert final.usage.completion_tokens == 12
        assert final.usage.tokens_per_sec > 0, final.usage.tokens_per_sec

    @pytest.mark.asyncio
    async def test_tool_use_without_argument_delta_sets_decode_tps(self):
        """无参工具不下发 input_json_delta：tool_use 块起点打点，TPS 为正。"""
        events = [
            {"type": "message_start", "message": {"usage": {"input_tokens": 10}}},
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Now"},
            },
            {"type": "content_block_stop", "index": 0},
            {
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 4},
            },
            {"type": "message_stop"},
        ]
        p = _anthropic_provider()
        await _replace_client(p, _anthropic_sse(events))
        acc = p.create_accumulator()

        final = None
        async for chunk in p._generate_stream({}, "claude-x", accumulator=acc):
            if chunk.content_blocks is not None:
                final = chunk

        state = acc.state
        assert isinstance(state, _StreamState)
        assert state.first_token_ts is not None, "tool_use 块起点必须打点首 token"
        assert final is not None
        assert final.usage.completion_tokens == 4
        assert final.usage.tokens_per_sec > 0, final.usage.tokens_per_sec

    @pytest.mark.asyncio
    async def test_text_stream_still_sets_decode_tps(self):
        """对照组：正文流的 TPS 语义不回归。"""
        events = [
            {"type": "message_start", "message": {"usage": {"input_tokens": 10}}},
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hello"},
            },
            {"type": "content_block_stop", "index": 0},
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 3},
            },
            {"type": "message_stop"},
        ]
        p = _anthropic_provider()
        await _replace_client(p, _anthropic_sse(events))
        acc = p.create_accumulator()

        final = None
        async for chunk in p._generate_stream({}, "claude-x", accumulator=acc):
            if chunk.content_blocks is not None:
                final = chunk

        state = acc.state
        assert isinstance(state, _StreamState)
        assert state.first_token_ts is not None
        assert final is not None
        assert final.usage.tokens_per_sec > 0, final.usage.tokens_per_sec
