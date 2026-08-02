# tests/test_anthropic_thinking_blocks.py
"""Anthropic 多块 thinking 忠实回放 + provider 生命周期 + OpenAI-compat 覆盖。

锁定行为（参考 test_anthropic_usage.py 的真实形态 payload 风格）：
- 多块 thinking + 各自 signature + 顺序的流式构建与回放 round-trip
- 无签名 thinking 降级 text（绝不给官方 Anthropic 发空/坏签名 thinking block）
- 存量兼容：develop 基线旧格式（content/reasoning_content/tool_calls）干净加载、
  对 Anthropic 回放不产生 thinking block、不报错
- redacted 块 round-trip
- 尾部 thinking 过滤
- 跨 provider 切模型不打断在途、旧 client 未关闭、切回同名复用
- OpenAI-compat 流事件映射 / 请求体序列化
"""

from __future__ import annotations

import json

import pytest

from wing.config import AgentConfig, Config, ProviderConfig
from wing.provider.anthropic import AnthropicProvider
from wing.provider.openai_compat import OpenAICompatProvider
from wing.schema import (
    Message,
    TextBlock,
    ThinkingBlock,
    ToolCall,
    ToolUseBlock,
)


# ─── SSE 伪造工具 ────────────────────────────────────────────────


class _FakeResponse:
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
    def __init__(self, lines: list[str]) -> None:
        self._lines = lines

    def build_request(self, *args: object, **kwargs: object) -> object:
        return object()

    async def send(self, request: object, stream: bool = False) -> _FakeResponse:
        return _FakeResponse(self._lines)


def _anthropic_sse(events: list[dict]) -> list[str]:
    lines: list[str] = []
    for ev in events:
        lines.append(f"event: {ev.get('type', 'unknown')}")
        lines.append(f"data: {json.dumps(ev)}")
        lines.append("")
    return lines


def _openai_sse(chunks: list[dict]) -> list[str]:
    lines: list[str] = []
    for c in chunks:
        lines.append(f"data: {json.dumps(c)}")
        lines.append("")
    lines.append("data: [DONE]")
    lines.append("")
    return lines


def _make_anthropic(extra_body: dict | None = None) -> AnthropicProvider:
    cfg = ProviderConfig(
        name="test-anthropic",
        protocol="anthropic",
        base_url="https://api.anthropic.com",
        api_key="sk-test",
        extra_body=extra_body
        if extra_body is not None
        else {"thinking": {"type": "enabled"}},
    )
    return AnthropicProvider(cfg)


# ─── 数据模型 / 存量兼容 ─────────────────────────────────────────


class TestContentBlockModel:
    def test_legacy_flat_message_builds_blocks(self):
        """develop 基线旧格式（无块数组/无 signature）能干净加载并映射出块数组。"""
        data = {
            "role": "assistant",
            "content": "hello",
            "reasoning_content": "hmm",
            "tool_calls": [{"id": "t1", "name": "Bash", "arguments": {"cmd": "ls"}}],
        }
        m = Message.model_validate(data)
        assert m.content_blocks is not None
        kinds = [type(b).__name__ for b in m.content_blocks]
        assert kinds == ["ThinkingBlock", "TextBlock", "ToolUseBlock"]
        # 旧数据 thinking 无 signature
        assert isinstance(m.content_blocks[0], ThinkingBlock)
        assert not m.content_blocks[0].has_valid_signature

    def test_legacy_ignores_unknown_reasoning_signature(self):
        """旧数据若残留 reasoning_signature 字段（extra=ignore）应被忽略，不报错。"""
        data = {
            "role": "assistant",
            "content": "hi",
            "reasoning_content": "r",
            "reasoning_signature": "stale-sig",
        }
        m = Message.model_validate(data)
        assert m.content_blocks is not None
        # thinking 块不带签名（stale-sig 被忽略）
        thinking = m.content_blocks[0]
        assert isinstance(thinking, ThinkingBlock)
        assert not thinking.has_valid_signature

    def test_blocks_round_trip_persistence(self):
        """块数组（含 per-block signature）经 model_dump/model_validate 无损 round-trip。"""
        m = Message(
            role="assistant",
            content_blocks=[
                ThinkingBlock(thinking="t1", signature="sig1"),
                ToolUseBlock(id="x", name="Read", input={"p": "/a"}),
                ThinkingBlock(thinking="t2", signature="sig2"),
                TextBlock(text="done"),
            ],
        )
        m2 = Message.model_validate(m.model_dump())
        blocks = m2.content_blocks
        assert blocks is not None
        assert isinstance(blocks[0], ThinkingBlock)
        assert blocks[0].signature == "sig1"
        assert isinstance(blocks[2], ThinkingBlock)
        assert blocks[2].signature == "sig2"
        assert isinstance(blocks[1], ToolUseBlock)
        assert blocks[1].input == {"p": "/a"}

    def test_sync_flat_from_blocks(self):
        """commit 时扁平字段由块数组填充。"""
        m = Message(
            role="assistant",
            content_blocks=[
                ThinkingBlock(thinking="th", signature="s"),
                TextBlock(text="hi"),
                ToolUseBlock(id="x", name="Read", input={"p": "/a"}),
            ],
        )
        m.sync_flat_from_blocks()
        assert m.content == "hi"
        assert m.reasoning_content == "th"
        assert m.tool_calls is not None
        assert m.tool_calls[0].name == "Read"

    def test_user_message_has_no_blocks(self):
        assert Message(role="user", content="q").content_blocks is None


# ─── Anthropic 回放序列化 ────────────────────────────────────────


class TestAnthropicReplay:
    @pytest.mark.asyncio
    async def test_multi_block_round_trip(self):
        """多块 thinking + 各自 signature + 顺序，回放一对一映射、字节不变。"""
        p = _make_anthropic()
        try:
            m = Message(
                role="assistant",
                content_blocks=[
                    ThinkingBlock(thinking="think 1", signature="SIG1"),
                    ToolUseBlock(id="t1", name="Bash", input={"cmd": "ls"}),
                    ThinkingBlock(thinking="think 2", signature="SIG2"),
                    TextBlock(text="done"),
                ],
            )
            ser = p._serialize_assistant(m)
            assert ser[0] == {
                "type": "thinking",
                "thinking": "think 1",
                "signature": "SIG1",
            }
            assert ser[1]["type"] == "tool_use"
            assert ser[1]["input"] == {"cmd": "ls"}
            assert ser[2] == {
                "type": "thinking",
                "thinking": "think 2",
                "signature": "SIG2",
            }
            assert ser[3] == {"type": "text", "text": "done"}
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_no_signature_degrades_to_text(self):
        """无有效 signature 的 thinking 降级为 text 块，绝不发 thinking block。"""
        p = _make_anthropic()
        try:
            m = Message(
                role="assistant",
                content_blocks=[
                    ThinkingBlock(thinking="old reasoning"),  # 无 signature
                    TextBlock(text="hi"),
                ],
            )
            ser = p._serialize_assistant(m)
            assert all(b["type"] != "thinking" for b in ser)
            assert ser[0] == {"type": "text", "text": "old reasoning"}
            assert ser[1] == {"type": "text", "text": "hi"}
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_empty_thinking_no_signature_dropped(self):
        """空 thinking 且无 signature → 整块丢弃。"""
        p = _make_anthropic()
        try:
            m = Message(
                role="assistant",
                content_blocks=[ThinkingBlock(thinking=""), TextBlock(text="hi")],
            )
            ser = p._serialize_assistant(m)
            assert ser == [{"type": "text", "text": "hi"}]
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_redacted_round_trip(self):
        """redacted 块回放为 redacted_thinking + data（黑盒原样）。"""
        p = _make_anthropic()
        try:
            m = Message(
                role="assistant",
                content_blocks=[
                    ThinkingBlock(
                        thinking="", signature="ENCRYPTEDBLOB", redacted=True
                    ),
                    TextBlock(text="hi"),
                ],
            )
            ser = p._serialize_assistant(m)
            assert ser[0] == {"type": "redacted_thinking", "data": "ENCRYPTEDBLOB"}
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_legacy_replay_no_thinking_block(self):
        """存量旧格式回放：不产生任何 thinking block（降级 text），不报错。"""
        p = _make_anthropic()
        try:
            m = Message.model_validate(
                {"role": "assistant", "content": "hi", "reasoning_content": "old"}
            )
            ser = p._serialize_assistant(m)
            assert all(b["type"] != "thinking" for b in ser)
            assert {"type": "text", "text": "old"} in ser
            assert {"type": "text", "text": "hi"} in ser
        finally:
            await p.aclose()

    def test_strip_trailing_thinking(self):
        """尾部 thinking 过滤：剥离尾部连续 thinking；全 thinking 插占位。"""
        # 尾部是 tool_use → 不变
        msgs = [
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "x", "signature": "s"},
                    {"type": "tool_use", "id": "t", "name": "n", "input": {}},
                ],
            }
        ]
        AnthropicProvider._strip_trailing_thinking(msgs)
        assert len(msgs[0]["content"]) == 2

        # 尾部 thinking 被剥离
        msgs2 = [
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "thinking", "thinking": "x", "signature": "s"},
                ],
            }
        ]
        AnthropicProvider._strip_trailing_thinking(msgs2)
        assert msgs2[0]["content"] == [{"type": "text", "text": "hi"}]

        # 全 thinking → 占位 text
        msgs3 = [
            {
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": "x", "signature": "s"}],
            }
        ]
        AnthropicProvider._strip_trailing_thinking(msgs3)
        assert msgs3[0]["content"] == [{"type": "text", "text": "[No message content]"}]

    @pytest.mark.asyncio
    async def test_cache_control_avoids_thinking(self):
        """cache_control 打在最后一个非 thinking 块上。"""
        msgs = [
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "thinking", "thinking": "x", "signature": "s"},
                ],
            }
        ]
        AnthropicProvider._apply_cache_control(msgs)
        assert "cache_control" not in msgs[0]["content"][1]
        assert msgs[0]["content"][0]["cache_control"] == {"type": "ephemeral"}


# ─── Anthropic 流式逐块构建 ──────────────────────────────────────


class TestAnthropicStreaming:
    @pytest.mark.asyncio
    async def test_stream_builds_ordered_blocks_with_signatures(self):
        """流式按 index 逐块建块，signature per-block 归位，产出有序块数组。"""
        events = [
            {
                "type": "message_start",
                "message": {"usage": {"input_tokens": 10, "output_tokens": 0}},
            },
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "think part 1"},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "SIG1"},
            },
            {"type": "content_block_stop", "index": 0},
            {
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Bash"},
            },
            {
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": '{"cmd":'},
            },
            {
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": '"ls"}'},
            },
            {"type": "content_block_stop", "index": 1},
            {
                "type": "content_block_start",
                "index": 2,
                "content_block": {"type": "thinking", "thinking": ""},
            },
            {
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "thinking_delta", "thinking": "think part 2"},
            },
            {
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "signature_delta", "signature": "SIG2"},
            },
            {"type": "content_block_stop", "index": 2},
            {
                "type": "content_block_start",
                "index": 3,
                "content_block": {"type": "text", "text": ""},
            },
            {
                "type": "content_block_delta",
                "index": 3,
                "delta": {"type": "text_delta", "text": "done"},
            },
            {"type": "content_block_stop", "index": 3},
            {"type": "message_delta", "usage": {"output_tokens": 5}},
            {"type": "message_stop"},
        ]
        p = _make_anthropic()
        await p._client.aclose()
        p._client = _FakeClient(_anthropic_sse(events))  # ty: ignore[invalid-assignment]

        final_blocks = None
        async for chunk in p._generate_stream(body={}, model="claude"):
            if chunk.content_blocks is not None:
                final_blocks = chunk.content_blocks

        assert final_blocks is not None
        assert len(final_blocks) == 4
        assert isinstance(final_blocks[0], ThinkingBlock)
        assert final_blocks[0].thinking == "think part 1"
        assert final_blocks[0].signature == "SIG1"
        assert isinstance(final_blocks[1], ToolUseBlock)
        assert final_blocks[1].input == {"cmd": "ls"}
        assert isinstance(final_blocks[2], ThinkingBlock)
        assert final_blocks[2].thinking == "think part 2"
        assert final_blocks[2].signature == "SIG2"
        assert isinstance(final_blocks[3], TextBlock)
        assert final_blocks[3].text == "done"


# ─── thinking 对外状态 ───────────────────────────────────────────


class TestThinkingStatus:
    @pytest.mark.asyncio
    async def test_status_from_extra_body_enabled(self):
        p = _make_anthropic({"thinking": {"type": "enabled", "budget_tokens": 100}})
        try:
            assert p.thinking is True
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_status_false_without_thinking_config(self):
        p = _make_anthropic({})
        try:
            assert p.thinking is False
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_interleaved_beta_header_when_thinking(self):
        headers = AnthropicProvider._make_headers(
            "sk", "2023-06-01", {"thinking": {"type": "enabled"}}
        )
        assert headers["anthropic-beta"] == "interleaved-thinking-2025-05-14"
        headers_off = AnthropicProvider._make_headers("sk", "2023-06-01", {})
        assert "anthropic-beta" not in headers_off


# ─── Provider 生命周期 ───────────────────────────────────────────


class TestProviderLifecycle:
    @pytest.fixture
    def sm(self):
        from wing.session_manager import SessionManager
        from wing.store import MemorySessionStore

        return SessionManager(
            {"memory": MemorySessionStore()}, default_backend="memory"
        )

    @pytest.mark.asyncio
    async def test_cross_provider_switch_keeps_old_client(self, sm, monkeypatch):
        """跨 provider 切模型：旧 client 不关闭（不打断在途）、切回同名复用。"""
        session = sm.create_session()
        old_provider = session.agent.model_provider
        assert old_provider.name == "default"

        two = Config(
            providers=[
                ProviderConfig(
                    name="default", base_url="https://a.example.com", api_key="k"
                ),
                ProviderConfig(
                    name="p2", base_url="https://b.example.com", api_key="k"
                ),
            ],
            agents=[AgentConfig(name="default", model="gpt-4", provider="default")],
        )
        monkeypatch.setattr("wing.session.get_config", lambda: two)

        # 跨 provider 切模型
        session._apply_model("model-2", provider_name="p2")
        assert session.agent.model_provider.name == "p2"
        # 旧 client 未被关闭（在途生成不被打断）
        assert old_provider._client.is_closed is False
        # 旧 provider 仍被有界持有
        assert session._providers["default"] is old_provider

        # 切回 default：复用缓存的 client（同一对象），而非新建
        session._apply_model("model-3", provider_name="default")
        assert session.agent.model_provider is old_provider

    @pytest.mark.asyncio
    async def test_same_provider_model_switch_no_new_client(self, sm):
        """同 provider 内切模型（provider_name 与当前相同）不创建新 provider。"""
        session = sm.create_session()
        original = session.agent.model_provider
        session._apply_model("new-model", provider_name="default")
        assert session.agent.model_provider is original
        assert session.agent.model == "new-model"


# ─── OpenAI-compat 覆盖 ──────────────────────────────────────────


class TestOpenAICompat:
    def _make(self) -> OpenAICompatProvider:
        cfg = ProviderConfig(
            name="test-openai",
            protocol="openai",
            base_url="https://api.example.com",
            api_key="sk-test",
        )
        return OpenAICompatProvider(cfg)

    @pytest.mark.asyncio
    async def test_stream_event_mapping(self):
        """OpenAI 流事件映射：content/reasoning 增量累积，tool_calls 终结解析。"""
        chunks = [
            {"choices": [{"delta": {"reasoning_content": "let me think"}}]},
            {"choices": [{"delta": {"content": "hello "}}]},
            {"choices": [{"delta": {"content": "world"}}]},
            {
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "c1",
                                    "function": {
                                        "name": "Bash",
                                        "arguments": '{"cmd":',
                                    },
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
                                {"index": 0, "function": {"arguments": '"ls"}'}}
                            ]
                        },
                        "finish_reason": "tool_calls",
                    }
                ]
            },
            {"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
        ]
        p = self._make()
        await p._client.aclose()
        p._client = _FakeClient(_openai_sse(chunks))  # ty: ignore[invalid-assignment]

        content_parts: list[str] = []
        reasoning_parts: list[str] = []
        tool_calls = None
        async for chunk in p._generate_stream(body={}, model="gpt-4"):
            if chunk.content:
                content_parts.append(chunk.content)
            if chunk.reasoning_content:
                reasoning_parts.append(chunk.reasoning_content)
            if chunk.tool_calls:
                tool_calls = chunk.tool_calls

        assert "".join(content_parts) == "hello world"
        assert "".join(reasoning_parts) == "let me think"
        assert tool_calls is not None
        assert tool_calls[0].name == "Bash"
        assert tool_calls[0].arguments == {"cmd": "ls"}

    @pytest.mark.asyncio
    async def test_build_body_serialization(self):
        """OpenAI 请求体序列化：messages 顺序 + tool_calls 转 OpenAI 格式。"""
        p = self._make()
        try:
            msgs = [
                Message(role="system", content="sys"),
                Message(role="user", content="hi"),
                Message(
                    role="assistant",
                    content="yo",
                    tool_calls=[
                        ToolCall(id="t1", name="Bash", arguments={"cmd": "ls"})
                    ],
                ),
            ]
            body = p._build_body(msgs, "gpt-4", None, False)
            assert body["model"] == "gpt-4"
            roles = [m["role"] for m in body["messages"]]
            assert roles == ["system", "user", "assistant"]
            tc = body["messages"][2]["tool_calls"][0]
            assert tc["function"]["name"] == "Bash"
            assert json.loads(tc["function"]["arguments"]) == {"cmd": "ls"}
        finally:
            await p.aclose()
