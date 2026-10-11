# tests/test_provider_ttft.py
"""首 token RT（``first_chunk_rt_ms``）口径回归——落在首个生成增量，而不是响应头。

背景：该字段曾在**响应头到达**处打点。对"先开流、后生成"的服务端（立即回 200 +
空 role 帧，排队 / 预填充之后才吐第一个 token）会报出几十毫秒，与用户体感的等待
（秒级）完全失真。修复：打点移到首个正文 / 思考 / tool 增量——与 decode TPS 的
``first_token_ts`` 同一时刻。

锁定：
- OpenAI：响应头早到、首个内容帧晚到 N ms → 带 token 的 usage 里
  ``first_chunk_rt_ms`` ≈ 头时延 + N，不是头时延；
- Anthropic：同一口径，以 ``message_stop`` 最终 usage 为准（增量帧的元信息
  usage 同值）；
- 兜底：流内无任何生成（只有空帧 / usage 帧）→ 退回首包 RT。

时钟：三个 provider 模块的 ``time`` 换成本文件的可控时钟（模块属性，不动全局
``time``）——延迟都靠伪造 SSE 的"帧前等待"推进，断言是确定值而非睡眠近似值。
"""

from __future__ import annotations

import json

import pytest

import wing.provider.anthropic.provider as anthropic_provider_module
import wing.provider.anthropic.stream as anthropic_stream_module
import wing.provider.openai.provider as openai_provider_module
from wing.config import ProviderConfig
from wing.provider.anthropic.provider import AnthropicProvider
from wing.provider.openai.provider import OpenAICompatProvider

# ─── 可控时钟与伪造传输 ──────────────────────────────────────────

#: 伪造响应头的到达时延（秒）：60ms 远小于首个内容帧的 800/900ms。
HEADER_DELAY = 0.06


class _Clock:
    """可控单调时钟（秒）。"""

    def __init__(self, start: float = 1000.0) -> None:
        self.now = start

    def monotonic(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


@pytest.fixture()
def clock(monkeypatch: pytest.MonkeyPatch) -> _Clock:
    clock = _Clock()
    monkeypatch.setattr(openai_provider_module, "time", clock)
    monkeypatch.setattr(anthropic_provider_module, "time", clock)
    monkeypatch.setattr(anthropic_stream_module, "time", clock)
    return clock


class _FakeResponse:
    is_error = False

    def __init__(self, lines: list[tuple[float, str]], clock: _Clock) -> None:
        self._lines = lines
        self._clock = clock
        self.headers = {"request-id": "ttft-rid", "x-request-id": "ttft-rid"}

    async def aiter_lines(self):
        for delay, line in self._lines:
            if delay:
                self._clock.advance(delay)
            yield line

    async def aclose(self) -> None:
        pass


class _FakeClient:
    """伪造 httpx：``send`` 推进头时延，逐行迭代时推进帧间延迟。"""

    def __init__(self, lines: list[tuple[float, str]], clock: _Clock) -> None:
        self._lines = lines
        self._clock = clock

    def build_request(self, *args: object, **kwargs: object) -> object:
        return object()

    async def send(self, request: object, stream: bool = False) -> _FakeResponse:
        self._clock.advance(HEADER_DELAY)
        return _FakeResponse(self._lines, self._clock)


async def _install(
    provider: OpenAICompatProvider | AnthropicProvider,
    lines: list[tuple[float, str]],
    clock: _Clock,
) -> None:
    """关闭真实 httpx client，替换为伪造流。"""
    await provider._client.aclose()
    provider._client = _FakeClient(lines, clock)  # ty: ignore[invalid-assignment]


def _openai_sse(chunks: list[tuple[float, dict]]) -> list[tuple[float, str]]:
    """OpenAI 帧序：(帧前延迟, chunk) → SSE 行。"""
    lines: list[tuple[float, str]] = []
    for delay, chunk in chunks:
        lines.append((delay, f"data: {json.dumps(chunk)}"))
        lines.append((0.0, ""))
    lines.append((0.0, "data: [DONE]"))
    lines.append((0.0, ""))
    return lines


def _anthropic_sse(events: list[tuple[float, dict]]) -> list[tuple[float, str]]:
    lines: list[tuple[float, str]] = []
    for delay, event in events:
        lines.append((delay, f"event: {event.get('type', 'unknown')}"))
        lines.append((0.0, f"data: {json.dumps(event)}"))
        lines.append((0.0, ""))
    return lines


def _openai_provider() -> OpenAICompatProvider:
    return OpenAICompatProvider(
        ProviderConfig(
            name="test-openai-ttft",
            protocol="openai",
            base_url="https://api.example.com",
            api_key="sk-test",
        )
    )


def _anthropic_provider() -> AnthropicProvider:
    return AnthropicProvider(
        ProviderConfig(
            name="test-anthropic-ttft",
            protocol="anthropic",
            base_url="https://api.anthropic.com",
            api_key="sk-test",
        )
    )


# ─── OpenAI-compat ──────────────────────────────────────────────


class TestOpenAITtft:
    @pytest.mark.asyncio
    async def test_ttft_lands_on_first_content_frame(self, clock: _Clock):
        """头 60ms 到、空 role 帧立即、首个内容帧 +800ms → ttft ≈ 860ms。"""
        chunks = [
            (0.0, {"choices": [{"delta": {"role": "assistant", "content": ""}}]}),
            (0.8, {"choices": [{"delta": {"content": "hi"}}]}),
            (
                0.05,
                {
                    "choices": [{"delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                },
            ),
        ]
        p = _openai_provider()
        await _install(p, _openai_sse(chunks), clock)
        acc = p.create_accumulator()

        token_usage = None
        async for chunk in p._generate_stream({}, "gpt-4", accumulator=acc):
            if chunk.usage.completion_tokens:
                token_usage = chunk.usage

        assert token_usage is not None
        ttft = token_usage.first_chunk_rt_ms
        assert ttft == pytest.approx((HEADER_DELAY + 0.8) * 1000, abs=1.0), ttft
        assert ttft > HEADER_DELAY * 1000 + 700, "口径必须是首内容帧，不是响应头"

    @pytest.mark.asyncio
    async def test_ttft_falls_back_to_header_without_generation(self, clock: _Clock):
        """流内无任何生成：退回首包 RT（响应头时刻）。"""
        chunks = [
            (0.0, {"choices": [{"delta": {"role": "assistant", "content": ""}}]}),
            (
                0.3,
                {"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 0}},
            ),
        ]
        p = _openai_provider()
        await _install(p, _openai_sse(chunks), clock)

        token_usage = None
        async for chunk in p._generate_stream({}, "gpt-4"):
            if chunk.usage.prompt_tokens:
                token_usage = chunk.usage

        assert token_usage is not None
        assert token_usage.first_chunk_rt_ms == pytest.approx(
            HEADER_DELAY * 1000, abs=1.0
        ), token_usage


# ─── Anthropic ──────────────────────────────────────────────────


class TestAnthropicTtft:
    @pytest.mark.asyncio
    async def test_ttft_lands_on_first_delta(self, clock: _Clock):
        """头 60ms 到、首个 thinking 增量 +900ms → message_stop 的 usage ≈ 960ms。"""
        events = [
            (0.0, {"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
            (
                0.0,
                {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "thinking"},
                },
            ),
            (
                0.9,
                {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "hmm"},
                },
            ),
            (0.0, {"type": "content_block_stop", "index": 0}),
            (
                0.0,
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 3},
                },
            ),
            (0.0, {"type": "message_stop"}),
        ]
        p = _anthropic_provider()
        await _install(p, _anthropic_sse(events), clock)

        delta_usage = None
        final_usage = None
        async for chunk in p._generate_stream({}, "claude-x"):
            if chunk.reasoning_content:
                delta_usage = chunk.usage
            if chunk.content_blocks is not None:
                final_usage = chunk.usage

        assert delta_usage is not None and final_usage is not None
        expected = pytest.approx((HEADER_DELAY + 0.9) * 1000, abs=1.0)
        assert final_usage.first_chunk_rt_ms == expected, final_usage
        assert delta_usage.first_chunk_rt_ms == expected, delta_usage

    @pytest.mark.asyncio
    async def test_ttft_falls_back_to_header_without_generation(self, clock: _Clock):
        """流内无任何 delta：退回首包 RT。"""
        events = [
            (0.0, {"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
            (
                0.25,
                {"type": "message_delta", "delta": {}, "usage": {"output_tokens": 0}},
            ),
            (0.0, {"type": "message_stop"}),
        ]
        p = _anthropic_provider()
        await _install(p, _anthropic_sse(events), clock)

        final_usage = None
        async for chunk in p._generate_stream({}, "claude-x"):
            if chunk.content_blocks is not None:
                final_usage = chunk.usage

        assert final_usage is not None
        assert final_usage.first_chunk_rt_ms == pytest.approx(
            HEADER_DELAY * 1000, abs=1.0
        ), final_usage
