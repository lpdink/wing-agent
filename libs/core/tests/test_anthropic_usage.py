# tests/test_anthropic_usage.py
"""Anthropic 流式 usage 统计回归测试。

背景：部分代理把「权威的累计 usage」放在 message_delta，而 message_start 里的
input_tokens 是冻住的骨架值（不随上下文增长），直接用它会让 total 虚高、
缓存命中率被低估（实测 86.7% vs 真实 ~99%）。另一些端点（标准 Anthropic）
则把完整 usage（含 cache_creation/cache_read）放在 message_start，message_delta
仅含 output_tokens。

本测试用真实抓到的 SSE payload 锁定两种形态的统计口径：
    total prompt_tokens = input_tokens + cache_read + cache_creation
（官方文档：三者互斥相加；input_tokens 永远是非缓存部分。）
"""

import json

import pytest

from wing.config import ProviderConfig
from wing.provider.anthropic import AnthropicProvider


class _FakeResponse:
    """伪造 httpx 流式响应：headers + aiter_lines + aclose。"""

    def __init__(self, lines: list[str]) -> None:
        self._lines = lines
        self.headers = {"request-id": "test-request-id"}

    def raise_for_status(self) -> None:
        pass

    async def aiter_lines(self):
        for line in self._lines:
            yield line

    async def aclose(self) -> None:
        pass


class _FakeClient:
    """伪造 httpx.AsyncClient：build_request + send(stream=True)。"""

    def __init__(self, lines: list[str]) -> None:
        self._lines = lines

    def build_request(self, *args: object, **kwargs: object) -> object:
        return object()

    async def send(self, request: object, stream: bool = False) -> _FakeResponse:
        return _FakeResponse(self._lines)


def _sse_lines(events: list[dict]) -> list[str]:
    """把事件 dict 列表渲染成 SSE 行流（event:/data: + 空行分隔）。"""
    lines: list[str] = []
    for ev in events:
        lines.append(f"event: {ev.get('type', 'unknown')}")
        lines.append(f"data: {json.dumps(ev)}")
        lines.append("")
    return lines


async def _run_usage(events: list[dict]):
    """驱动 _generate_stream，返回最终（message_stop）usage。"""
    cfg = ProviderConfig(
        name="test-anthropic",
        protocol="anthropic",
        base_url="https://api.anthropic.com",
        api_key="sk-test",
    )
    provider = AnthropicProvider(cfg)
    await provider._client.aclose()  # 关掉不会用到的真实 httpx client
    provider._client = _FakeClient(_sse_lines(events))  # ty: ignore[invalid-assignment]

    final = None
    async for chunk in provider._generate_stream(body={}, model="claude-test"):
        if chunk.usage.prompt_tokens or chunk.usage.completion_tokens:
            final = chunk.usage

    assert final is not None, "no usage chunk emitted"
    return final


class TestAnthropicUsageAccounting:
    @pytest.mark.asyncio
    async def test_proxy_authoritative_message_delta(self):
        """代理形态：message_start.input_tokens 冻住(9587)，权威值在 message_delta(6)。

        真实抓包：命中率曾被算成 86.7%，修正后应为 ~99.5%。
        """
        events = [
            {
                "type": "message_start",
                "message": {"usage": {"input_tokens": 9587, "output_tokens": 0}},
            },
            {
                "type": "message_delta",
                "usage": {
                    "input_tokens": 6,
                    "output_tokens": 158,
                    "cache_creation_input_tokens": 334,
                    "cache_read_input_tokens": 75092,
                    "prompt_tokens_details": {"cached_tokens": 75092},
                },
            },
            {"type": "message_stop"},
        ]
        usage = await _run_usage(events)

        # 权威 input_tokens 取自 message_delta(6)，total = 6 + 75092 + 334
        assert usage.prompt_tokens == 75432
        assert usage.cached_tokens == 75092
        assert usage.completion_tokens == 158
        # 锁定核心 bug：若误用冻住的 message_start(9587)，total 会是 85013
        assert usage.prompt_tokens != 9587 + 75092 + 334

    @pytest.mark.asyncio
    async def test_vanilla_message_start_fallback(self):
        """标准形态：cache 字段在 message_start，message_delta 仅含 output_tokens。

        验证 message_delta 不带 input_tokens 时回退 message_start，且
        cache_creation 从 message_start 读取（此前该分支漏读，恒为 0）。
        """
        events = [
            {
                "type": "message_start",
                "message": {
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 1,
                        "cache_creation_input_tokens": 500,
                        "cache_read_input_tokens": 2000,
                    }
                },
            },
            {"type": "message_delta", "usage": {"output_tokens": 50}},
            {"type": "message_stop"},
        ]
        usage = await _run_usage(events)

        # total = 100 + 2000 + 500（cache_creation 必须被计入）
        assert usage.prompt_tokens == 2600
        assert usage.cached_tokens == 2000
        # message_delta 的 output_tokens 覆盖 message_start 初值
        assert usage.completion_tokens == 50

    @pytest.mark.asyncio
    async def test_first_turn_cache_write_no_false_hit(self):
        """首轮写缓存：cache_read=0、cache_creation 大，命中率应为 0（无虚报命中）。"""
        events = [
            {
                "type": "message_start",
                "message": {"usage": {"input_tokens": 9587, "output_tokens": 0}},
            },
            {
                "type": "message_delta",
                "usage": {
                    "input_tokens": 6,
                    "output_tokens": 152,
                    "cache_creation_input_tokens": 75092,
                    "cache_read_input_tokens": 0,
                },
            },
            {"type": "message_stop"},
        ]
        usage = await _run_usage(events)

        # total = 6 + 0 + 75092
        assert usage.prompt_tokens == 75098
        assert usage.cached_tokens == 0
        assert usage.completion_tokens == 152
