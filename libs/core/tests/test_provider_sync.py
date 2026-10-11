"""非流式补全路径（`_generate_sync`）与请求体构造。

流式是主路径，但压缩（Compactor）等场景走 `stream=False`——这条路径此前没有
任何测试。用替身 client 直接喂响应体，锁定三件事：

- 响应解析：content block / tool call / usage 口径（Anthropic 的 total 输入
  含缓存读写，与流式路径同口径）；
- 错误面：HTTP 4xx/5xx 抛 ProviderHTTPError（携带 body）、请求超时抛 TimeoutError；
- 请求体：system 的两种形态（显式缓存模式下的 block + cache_control）与
  thinking 预算的非法组合剥离。
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from wing.config import ProviderConfig
from wing.provider.anthropic.provider import AnthropicProvider
from wing.provider.openai.provider import OpenAICompatProvider
from wing.provider.base import RequestOptions
from wing.provider.transport import ProviderHTTPError
from wing.schema import Message


class _FakeResponse:
    """httpx.Response 替身：只实现 raise_with_body / 解析用到的那几个面。"""

    def __init__(
        self,
        payload: dict | None = None,
        *,
        status_code: int = 200,
        headers: dict[str, str] | None = None,
        text: str = "",
    ) -> None:
        self._payload = payload if payload is not None else {}
        self.status_code = status_code
        self.headers = headers or {}
        self.text = text
        self.url = "https://api.example.com/v1/messages"

    @property
    def is_error(self) -> bool:
        return self.status_code >= 400

    def json(self) -> dict:
        return self._payload

    async def aread(self) -> bytes:
        return self.text.encode()


class _FakeClient:
    """httpx.AsyncClient 替身：`post` 返回脚本化响应或抛脚本化异常。"""

    def __init__(
        self, response: _FakeResponse | None = None, error: Exception | None = None
    ):
        self._response = response
        self._error = error
        self.calls: list[dict[str, Any]] = []

    async def post(
        self, path: str, json: dict | None = None, headers: dict | None = None
    ):
        self.calls.append({"path": path, "json": json, "headers": headers})
        if self._error is not None:
            raise self._error
        assert self._response is not None
        return self._response


def _provider(cls, **overrides: Any):
    cfg = ProviderConfig(
        name="test-provider",
        base_url="https://api.example.com",
        api_key="sk-test",
        **overrides,
    )
    provider = cls(cfg)
    return provider


async def _install(provider, response=None, error=None) -> _FakeClient:
    """关掉真实 httpx client，装上替身（不必真的联网）。"""
    await provider._client.aclose()
    fake = _FakeClient(response, error)
    provider._client = fake  # ty: ignore[invalid-assignment]
    return fake


async def _sync_items(agen) -> list:
    return [item async for item in agen]


# ── Anthropic ─────────────────────────────────────────────────


def _anthropic_payload() -> dict:
    return {
        "id": "msg_1",
        "stop_reason": "tool_use",
        "content": [
            {"type": "thinking", "thinking": "let me think", "signature": "sig-1"},
            {"type": "text", "text": "answering "},
            {"type": "text", "text": "in two parts"},
            {
                "type": "tool_use",
                "id": "call-1",
                "name": "Bash",
                "input": {"cmd": "ls"},
            },
        ],
        "usage": {
            "input_tokens": 10,
            "output_tokens": 25,
            "cache_read_input_tokens": 100,
            "cache_creation_input_tokens": 5,
        },
    }


class TestAnthropicSync:
    @pytest.mark.asyncio
    async def test_parses_blocks_tools_and_usage(self):
        provider = _provider(AnthropicProvider)
        await _install(
            provider,
            _FakeResponse(_anthropic_payload(), headers={"request-id": "req-9"}),
        )

        items = await _sync_items(provider._generate_sync({}, "claude-x"))
        assert len(items) == 1
        resp = items[0]

        assert resp.content == "answering in two parts"
        assert resp.reasoning_content == "let me think"
        assert [type(b).__name__ for b in resp.content_blocks or []] == [
            "ThinkingBlock",
            "TextBlock",
            "TextBlock",
            "ToolUseBlock",
        ]
        assert resp.tool_calls is not None
        assert (resp.tool_calls[0].id, resp.tool_calls[0].name) == ("call-1", "Bash")
        assert resp.tool_calls[0].arguments == {"cmd": "ls"}

        # total 输入 = input + cache_read + cache_creation（与流式路径同口径）
        assert resp.usage.prompt_tokens == 115
        assert resp.usage.completion_tokens == 25
        assert resp.usage.cached_tokens == 100
        assert resp.usage.model == "claude-x"
        assert resp.usage.request_id == "req-9"
        assert resp.usage.stop_reason == "tool_use"
        assert resp.usage.first_chunk_rt_ms >= 0

    @pytest.mark.asyncio
    async def test_redacted_thinking_replayed_opaque(self):
        """redacted_thinking 当不透明黑盒：payload 存进 signature，原样回放。"""
        provider = _provider(AnthropicProvider)
        payload = {
            "content": [{"type": "redacted_thinking", "data": "encrypted-blob"}],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }
        await _install(provider, _FakeResponse(payload))

        items = await _sync_items(provider._generate_sync({}, "claude-x"))
        (block,) = items[0].content_blocks or []
        assert (block.thinking, block.signature, block.redacted) == (
            "",
            "encrypted-blob",
            True,
        )
        assert items[0].content is None
        assert items[0].reasoning_content is None

    @pytest.mark.asyncio
    async def test_zero_info_blocks_dropped(self):
        """空 text / 空 thinking 块剔除（与流式、OpenAI 路径对齐）。"""
        provider = _provider(AnthropicProvider)
        payload = {
            "content": [
                {"type": "text", "text": ""},
                {"type": "thinking", "thinking": "", "signature": None},
                {"type": "text", "text": "real"},
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }
        await _install(provider, _FakeResponse(payload))

        items = await _sync_items(provider._generate_sync({}, "claude-x"))
        assert [type(b).__name__ for b in items[0].content_blocks or []] == [
            "TextBlock"
        ]

    @pytest.mark.asyncio
    async def test_empty_content_yields_empty_block_array(self):
        """零内容响应是合法协议输出：块数组为空（轮有效性由 ReActLoop 判）。"""
        provider = _provider(AnthropicProvider)
        await _install(provider, _FakeResponse({"content": [], "usage": {}}))

        items = await _sync_items(provider._generate_sync({}, "claude-x"))
        assert items[0].content_blocks == []
        assert items[0].tool_calls is None
        assert items[0].usage.prompt_tokens == 0

    @pytest.mark.asyncio
    async def test_http_error_carries_body(self):
        provider = _provider(AnthropicProvider)
        await _install(
            provider,
            _FakeResponse(status_code=529, text='{"error":"overloaded"}'),
        )

        with pytest.raises(ProviderHTTPError) as excinfo:
            await _sync_items(provider._generate_sync({}, "claude-x"))

        assert excinfo.value.status_code == 529
        assert "overloaded" in excinfo.value.body

    @pytest.mark.asyncio
    async def test_request_timeout_maps_to_timeout_error(self):
        provider = _provider(AnthropicProvider, timeout_total=0.01)
        await _install(provider, error=asyncio.TimeoutError())

        with pytest.raises(TimeoutError, match="LLM request timeout"):
            await _sync_items(provider._generate_sync({}, "claude-x"))

    @pytest.mark.asyncio
    async def test_generate_routes_non_stream_to_sync(self):
        """`_generate(stream=False)` 走非流式分支，body 里 stream=False。"""
        provider = _provider(AnthropicProvider)
        fake = await _install(provider, _FakeResponse(_anthropic_payload()))

        items = await _sync_items(
            provider._generate(
                [Message(role="user", content="hi")],
                "claude-x",
                tools=None,
                stream=False,
                options=RequestOptions(thinking=True),
            )
        )

        assert items and fake.calls[0]["path"] == "/v1/messages"
        assert fake.calls[0]["json"]["stream"] is False
        # 会话级 thinking 开关：每请求动态 header + 请求体 override
        assert fake.calls[0]["headers"] == {
            "anthropic-beta": "interleaved-thinking-2025-05-14"
        }
        assert fake.calls[0]["json"]["thinking"]["type"] == "enabled"


class TestAnthropicBodyConstruction:
    """请求体：system 两种形态 + thinking 预算的非法组合剥离。"""

    def _messages(self) -> list[Message]:
        return [
            Message(role="system", content="you are wing"),
            Message(role="user", content="hi"),
        ]

    def test_system_as_plain_string(self):
        # explicit_cache_mode 默认 True——关掉它才是裸字符串形态
        provider = _provider(AnthropicProvider, explicit_cache_mode=False)
        body = provider._build_body(self._messages(), "claude-x", None, stream=False)
        assert body["system"] == "you are wing"

    def test_explicit_cache_mode_marks_system_block(self):
        provider = _provider(AnthropicProvider, explicit_cache_mode=True)
        body = provider._build_body(self._messages(), "claude-x", None, stream=False)
        assert body["system"] == [
            {
                "type": "text",
                "text": "you are wing",
                "cache_control": {"type": "ephemeral"},
            }
        ]

    def test_disabled_thinking_strips_budget(self):
        """Anthropic 校验：非 enabled 的 thinking 不得带 budget_tokens。"""
        provider = _provider(
            AnthropicProvider,
            extra_body={"thinking": {"type": "disabled", "budget_tokens": 4096}},
        )
        body = provider._build_body(
            [Message(role="user", content="hi")], "m", None, stream=False
        )
        assert body["thinking"] == {"type": "disabled"}
        # 只剥 per-request 视图，共享源不被写穿
        assert provider._extra_body["thinking"]["budget_tokens"] == 4096

    def test_enabled_thinking_keeps_budget(self):
        provider = _provider(
            AnthropicProvider,
            extra_body={"thinking": {"type": "enabled", "budget_tokens": 2048}},
        )
        body = provider._build_body(
            [Message(role="user", content="hi")], "m", None, stream=False
        )
        assert body["thinking"] == {"type": "enabled", "budget_tokens": 2048}


# ── OpenAI 兼容 ───────────────────────────────────────────────


def _openai_payload() -> dict:
    return {
        "choices": [
            {
                "finish_reason": "tool_calls",
                "message": {
                    "reasoning_content": "thinking hard",
                    "content": "hello",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "function": {"name": "Bash", "arguments": '{"cmd": "ls"}'},
                        },
                        {
                            "id": "call-2",
                            "function": {"name": "Read", "arguments": '{"path": "x",}'},
                        },
                    ],
                },
            }
        ],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 30,
            "prompt_tokens_details": {"cached_tokens": 8},
        },
    }


class TestOpenAISync:
    @pytest.mark.asyncio
    async def test_parses_message_tools_and_usage(self):
        provider = _provider(OpenAICompatProvider)
        await _install(
            provider,
            _FakeResponse(_openai_payload(), headers={"x-request-id": "req-7"}),
        )

        items = await _sync_items(provider._generate_sync({}, "gpt-x"))
        resp = items[0]

        assert resp.content == "hello"
        assert resp.reasoning_content == "thinking hard"
        assert resp.usage.prompt_tokens == 12
        assert resp.usage.completion_tokens == 30
        assert resp.usage.cached_tokens == 8
        assert resp.usage.request_id == "req-7"
        assert resp.usage.stop_reason == "tool_calls"
        assert resp.usage.tokens_per_sec > 0

        # 合法 args 正常解析；非法 args 不抛异常，错误现场随 ToolCall 传导
        assert resp.tool_calls is not None
        good, bad = resp.tool_calls
        assert good.arguments == {"cmd": "ls"} and good.arguments_error is None
        assert bad.arguments == {} and bad.arguments_error is not None
        assert '{"path": "x",}' in bad.arguments_error

    @pytest.mark.asyncio
    async def test_missing_usage_and_tools(self):
        provider = _provider(OpenAICompatProvider)
        payload = {"choices": [{"message": {"content": "only text"}}]}
        await _install(provider, _FakeResponse(payload))

        items = await _sync_items(provider._generate_sync({}, "gpt-x"))
        assert items[0].tool_calls is None
        assert items[0].usage.prompt_tokens == 0
        assert items[0].usage.stop_reason is None

    @pytest.mark.asyncio
    async def test_http_error_carries_body(self):
        provider = _provider(OpenAICompatProvider)
        await _install(provider, _FakeResponse(status_code=500, text="server exploded"))

        with pytest.raises(ProviderHTTPError) as excinfo:
            await _sync_items(provider._generate_sync({}, "gpt-x"))
        assert excinfo.value.status_code == 500
        assert excinfo.value.body == "server exploded"

    @pytest.mark.asyncio
    async def test_request_timeout_maps_to_timeout_error(self):
        provider = _provider(OpenAICompatProvider, timeout_total=0.01)
        await _install(provider, error=asyncio.TimeoutError())

        with pytest.raises(TimeoutError, match="LLM request timeout"):
            await _sync_items(provider._generate_sync({}, "gpt-x"))

    @pytest.mark.asyncio
    async def test_generate_routes_non_stream_to_sync(self):
        provider = _provider(OpenAICompatProvider)
        fake = await _install(provider, _FakeResponse(_openai_payload()))

        items = await _sync_items(
            provider._generate(
                [Message(role="user", content="hi")],
                "gpt-x",
                tools=None,
                stream=False,
            )
        )

        assert items and fake.calls[0]["path"] == "/chat/completions"
        assert fake.calls[0]["json"]["stream"] is False
        assert fake.calls[0]["json"]["enable_thinking"] is True
