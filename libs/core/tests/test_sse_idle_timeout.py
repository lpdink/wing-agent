"""流式响应体闲置超时契约（`lines_with_idle_timeout`，120s 硬编码阈值）。

背景（2026-09-13 实测）：响应头秒回之后上游 12.6 分钟一个字节都不发，而
`timeout_first_chunk` 只包住等响应头、`parse_sse_stream(resp.aiter_lines())`
没有任何应用层超时——唯一兜底是 httpx 的 `read=1200s`，只要上游偶发吐字节
就永不触发。turn 因此无界挂在 reasoning 中间。

本文件钉死：
- 闲置判定的语义（读取间隔，而不是总时长）；
- 超时抛内建 `TimeoutError`（交由既有 with_retry，不另写重试）；
- 两个 provider 都经共享 helper 读取响应体（同构）；
- 「停滞 → 重试」在真实 provider 路径上可自愈。
"""

from __future__ import annotations

import asyncio
import json

import pytest

from wing.config import ProviderConfig
from wing.provider.anthropic import AnthropicProvider
from wing.provider.openai_compat import OpenAICompatProvider
from wing.provider.sse import STREAM_IDLE_TIMEOUT, lines_with_idle_timeout

# 测试阈值：毫秒级，绝不真的等 120 秒。
FAST = 0.05


def _lines(items: list[str]):
    """普通（不阻塞）的行来源。"""

    async def gen():
        for item in items:
            yield item

    return gen()


def _stalling_lines(first: list[str]):
    """先给出 `first` 行，然后永久挂起（模拟"响应头已到、再无一字节"）。"""

    async def gen():
        for item in first:
            yield item
        await asyncio.Event().wait()  # 永不 set：停滞

    return gen()


class TestLinesWithIdleTimeout:
    @pytest.mark.asyncio
    async def test_passthrough_and_normal_eof(self):
        got = [line async for line in lines_with_idle_timeout(_lines(["a", "b"]))]
        assert got == ["a", "b"]

    @pytest.mark.timeout(10)  # 回归（缺闲置超时）会挂起，而不是失败
    @pytest.mark.asyncio
    async def test_stall_raises_builtin_timeout_error(self):
        stream = lines_with_idle_timeout(
            _stalling_lines(["a"]), timeout=FAST, context="openai_compat test-model"
        )
        assert await anext(stream) == "a"
        with pytest.raises(TimeoutError) as err:
            await anext(stream)
        # 文案必须说清"谁在什么之后停滞"——现场那条空消息异常的反面。
        text = str(err.value)
        assert "openai_compat test-model" in text
        assert "stalled" in text
        await stream.aclose()

    @pytest.mark.asyncio
    async def test_slow_but_alive_stream_is_not_killed(self):
        """判据是**读取间隔**而非总时长：持续吐字节的慢流不触发。"""

        async def gen():
            for i in range(5):
                await asyncio.sleep(FAST / 5)
                yield f"line-{i}"

        got = [line async for line in lines_with_idle_timeout(gen(), timeout=FAST)]
        assert got == [f"line-{i}" for i in range(5)]

    @pytest.mark.asyncio
    async def test_aclose_after_timeout_is_safe(self):
        stream = lines_with_idle_timeout(_stalling_lines([]), timeout=FAST)
        with pytest.raises(TimeoutError):
            await anext(stream)
        await stream.aclose()  # 幂等：超时后上游可能仍在读

    def test_default_threshold_is_120s(self):
        """阈值硬编码 120s（无配置项）——本 change 的决策。"""
        assert STREAM_IDLE_TIMEOUT == 120.0


# ============================================================
# Provider 端到端：两个协议都经共享 helper
# ============================================================


class _FakeResponse:
    """伪造 httpx 流式响应：headers + aiter_lines + aclose。"""

    is_error = False

    def __init__(self, items: list[str], stall: bool = False) -> None:
        self._items = items
        self._stall = stall
        self.headers = {"x-request-id": "req", "request-id": "req"}
        self.closed = False

    async def aiter_lines(self):
        for item in self._items:
            yield item
        if self._stall:
            await asyncio.Event().wait()

    async def aclose(self) -> None:
        self.closed = True


class _ScriptedClient:
    """按顺序返回脚本化的响应（第一次停滞、第二次正常……）。"""

    def __init__(self, responses: list[_FakeResponse]) -> None:
        self._responses = responses
        self.attempts = 0

    def build_request(self, *args: object, **kwargs: object) -> object:
        return object()

    async def send(self, request: object, stream: bool = False) -> _FakeResponse:
        response = self._responses[min(self.attempts, len(self._responses) - 1)]
        self.attempts += 1
        return response


def _oai_chunk(content: str) -> list[str]:
    return [f"data: {json.dumps({'choices': [{'delta': {'content': content}}]})}", ""]


def _anthropic_chunk(text: str) -> list[str]:
    events = [
        {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
        {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": text},
        },
        {"type": "content_block_stop", "index": 0},
        {"type": "message_stop"},
    ]
    out: list[str] = []
    for ev in events:
        out.append(f"event: {ev['type']}")
        out.append(f"data: {json.dumps(ev)}")
        out.append("")
    return out


def _openai_provider(client: _ScriptedClient, **config_kwargs: object):
    cfg = ProviderConfig(
        name="test-openai",
        protocol="openai",
        base_url="https://api.example.com/v1",
        api_key="sk-test",
        **config_kwargs,  # ty: ignore[invalid-argument-type]
    )
    provider = OpenAICompatProvider(cfg)
    provider._client = client  # ty: ignore[invalid-assignment]
    return provider


def _anthropic_provider(client: _ScriptedClient, **config_kwargs: object):
    cfg = ProviderConfig(
        name="test-anthropic",
        protocol="anthropic",
        base_url="https://api.anthropic.com",
        api_key="sk-test",
        **config_kwargs,  # ty: ignore[invalid-argument-type]
    )
    provider = AnthropicProvider(cfg)
    provider._client = client  # ty: ignore[invalid-assignment]
    return provider


class TestOpenAIStreamIdleTimeout:
    @pytest.mark.timeout(10)  # 回归（缺闲置超时）会挂起，而不是失败
    @pytest.mark.asyncio
    async def test_stalled_body_times_out(self, monkeypatch: pytest.MonkeyPatch):
        monkeypatch.setattr("wing.provider.openai_compat.STREAM_IDLE_TIMEOUT", FAST)
        response = _FakeResponse(_oai_chunk("hel"), stall=True)
        provider = _openai_provider(_ScriptedClient([response]))

        with pytest.raises(TimeoutError) as err:
            async for _ in provider._generate_stream(body={}, model="test-model"):
                pass

        assert "openai_compat test-model" in str(err.value)
        assert response.closed, "响应体必须在失败路径上被关闭"

    @pytest.mark.timeout(10)  # 回归（缺闲置超时）会挂起，而不是失败
    @pytest.mark.asyncio
    async def test_stall_then_retry_succeeds_via_existing_with_retry(
        self, monkeypatch: pytest.MonkeyPatch
    ):
        """停滞 → 既有 with_retry —— 不新增任何重试逻辑。"""
        monkeypatch.setattr("wing.provider.openai_compat.STREAM_IDLE_TIMEOUT", FAST)
        stalled = _FakeResponse(_oai_chunk("partial"), stall=True)
        good = _FakeResponse(_oai_chunk("hello"))
        client = _ScriptedClient([stalled, good])
        provider = _openai_provider(
            client, max_retries=3, max_retry_delay=0.01, timeout_first_chunk=5.0
        )

        chunks = [
            chunk
            async for chunk in provider.generate(
                messages=[], model="test-model", stream=True
            )
        ]

        assert client.attempts == 2, "首次停滞必须触发第二次尝试"
        assert any(chunk.content == "hello" for chunk in chunks)
        assert stalled.closed, "被放弃的那次尝试必须关闭响应体"


class TestAnthropicStreamIdleTimeout:
    @pytest.mark.timeout(10)  # 回归（缺闲置超时）会挂起，而不是失败
    @pytest.mark.asyncio
    async def test_stalled_body_times_out(self, monkeypatch: pytest.MonkeyPatch):
        monkeypatch.setattr("wing.provider.anthropic.STREAM_IDLE_TIMEOUT", FAST)
        response = _FakeResponse(_anthropic_chunk("hel"), stall=True)
        provider = _anthropic_provider(_ScriptedClient([response]))

        with pytest.raises(TimeoutError) as err:
            async for _ in provider._generate_stream(body={}, model="claude-test"):
                pass

        assert "anthropic claude-test" in str(err.value)
        assert response.closed, "响应体必须在失败路径上被关闭"

    @pytest.mark.asyncio
    async def test_healthy_stream_unaffected(self, monkeypatch: pytest.MonkeyPatch):
        """非停滞路径不受影响：正常块照常产出（同构不改语义）。"""
        monkeypatch.setattr("wing.provider.anthropic.STREAM_IDLE_TIMEOUT", FAST)
        provider = _anthropic_provider(
            _ScriptedClient([_FakeResponse(_anthropic_chunk("hi"))])
        )
        chunks = []
        async for chunk in provider._generate_stream(body={}, model="claude-test"):
            chunks.append(chunk)
        assert any(chunk.content == "hi" for chunk in chunks)
