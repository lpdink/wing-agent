# tests/test_anthropic_thinking_blocks.py
"""Anthropic 多块 thinking 忠实回放 + provider 生命周期 + OpenAI-compat 覆盖。

锁定行为（参考 test_anthropic_usage.py 的真实形态 payload 风格）：
- 多块 thinking + 各自 signature + 顺序的流式构建与回放 round-trip
- 无签名 thinking 原样回放（发空签名 signature:""，MUST NOT 降级 text）——
  此类数据产生于不下发签名的推理 provider 或存量旧会话，回放官方 Anthropic
  被拒是预期行为
- 存量兼容：develop 基线旧格式（content/reasoning_content/tool_calls）干净加载、
  回放保持 thinking 原样
- redacted 块 round-trip
- cache_control 与 OpenAI 路径同构（最后一个 block，不规避 thinking）
- 跨 provider 切模型不打断在途、旧 client 未关闭、切回同名复用
- OpenAI-compat 流事件映射 / 权威块数组产出 / 请求体序列化
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
    is_error = False

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
        assert m.content_blocks[0].signature is None

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
        assert thinking.signature is None

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
    async def test_no_signature_replays_with_empty_signature(self):
        """无 signature 的 thinking 原样回放（发空签名），MUST NOT 降级 text。"""
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
            assert ser[0] == {
                "type": "thinking",
                "thinking": "old reasoning",
                "signature": "",
            }
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
    async def test_legacy_replay_preserves_thinking(self):
        """存量旧格式回放：thinking 保持原样（空签名），不降级 text。

        旧数据非官方 Anthropic 产出（官方必下发签名），回放官方被拒是预期；
        回放不下发签名的推理 provider 则保真。
        """
        p = _make_anthropic()
        try:
            m = Message.model_validate(
                {"role": "assistant", "content": "hi", "reasoning_content": "old"}
            )
            ser = p._serialize_assistant(m)
            assert ser[0] == {"type": "thinking", "thinking": "old", "signature": ""}
            assert ser[1] == {"type": "text", "text": "hi"}
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_zero_block_assistant_dropped_from_request(self):
        """序列化零块的 assistant 整条丢弃——content:[] 会使本次及该
        session 后续所有请求 400（Anthropic 要求 content 至少一个块）。

        真实路径：纯 thinking 轮的 thinking 块被 clear_reasoning 剥离
        （preserved_thinking: false）。丢弃是配对安全的：零块即无
        tool_use，不会有后续 tool_result 引用本条。
        """
        p = _make_anthropic()
        try:
            thinking_only = Message(
                role="assistant",
                content_blocks=[ThinkingBlock(thinking="hmm", signature="s")],
            )
            # clear_reasoning 剥离 thinking 块 → 块数组为空
            thinking_only.content_blocks = None

            _, am = p._serialize_messages(
                [
                    Message(role="user", content="q1"),
                    thinking_only,
                    Message(role="user", content="q2"),
                ]
            )
            # 零块 assistant 被丢弃；两个 user 按严格交替规则合并
            assert [m["role"] for m in am] == ["user"]
            assert all(m["content"] for m in am), "不得出现空 content"

            # 配对安全：带 tool_use 的 assistant 不受丢弃逻辑影响
            _, am2 = p._serialize_messages(
                [
                    Message(role="user", content="q"),
                    Message(
                        role="assistant",
                        content_blocks=[
                            ToolUseBlock(id="t1", name="Bash", input={"cmd": "ls"})
                        ],
                    ),
                    Message(role="tool", tool_call_id="t1", content="ok"),
                ]
            )
            assert [m["role"] for m in am2] == ["user", "assistant", "user"]
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_cache_control_last_block_isomorphic(self):
        """cache_control 打在最后一个 block 上（与 OpenAI 路径同构，不规避 thinking）。"""
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
        assert "cache_control" not in msgs[0]["content"][0]
        assert msgs[0]["content"][1]["cache_control"] == {"type": "ephemeral"}


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


# ─── 流式 per-index 结构化 + 错误面 ─────────────────────────────


class TestStreamPerIndexAndErrors:
    @pytest.mark.asyncio
    async def test_interleaved_tool_deltas_not_corrupted(self):
        """两个 tool_use 块的 input_json_delta 交错到达 → 按 index 独立累积。"""
        events = [
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "t0", "name": "Bash"},
            },
            {
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Read"},
            },
            # 交错 delta：index 0 与 index 1 交替
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": '{"cmd":'},
            },
            {
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": '{"path":'},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": '"ls"}'},
            },
            {
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": '"/a"}'},
            },
            # stop 乱序：先停 index 1，再停 index 0
            {"type": "content_block_stop", "index": 1},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_stop"},
        ]
        p = _make_anthropic()
        await p._client.aclose()
        p._client = _FakeClient(_anthropic_sse(events))  # ty: ignore[invalid-assignment]

        finals: dict[str, dict] = {}
        blocks = None
        async for chunk in p._generate_stream(body={}, model="claude"):
            for tc in chunk.tool_calls or []:
                finals[tc.name] = tc.arguments
            if chunk.content_blocks is not None:
                blocks = chunk.content_blocks

        assert finals["Bash"] == {"cmd": "ls"}
        assert finals["Read"] == {"path": "/a"}
        assert blocks is not None and len(blocks) == 2
        assert blocks[0].input == {"cmd": "ls"}  # ty: ignore[unresolved-attribute]
        assert blocks[1].input == {"path": "/a"}  # ty: ignore[unresolved-attribute]

    @pytest.mark.asyncio
    async def test_stream_error_event_raises(self):
        """流中 error 事件抛 ProviderStreamError（触发重试，截断轮次不提交）。"""
        from wing.provider.errors import ProviderStreamError

        events = [
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "partial"},
            },
            {
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"},
            },
        ]
        p = _make_anthropic()
        await p._client.aclose()
        p._client = _FakeClient(_anthropic_sse(events))  # ty: ignore[invalid-assignment]

        with pytest.raises(ProviderStreamError, match="Overloaded"):
            async for _ in p._generate_stream(body={}, model="claude"):
                pass

    @pytest.mark.asyncio
    async def test_http_error_carries_body(self):
        """4xx 响应 body 进异常消息（max_tokens 超限等关键信息的所在）。"""
        from wing.provider.errors import ProviderHTTPError, raise_with_body

        class _ErrResponse:
            is_error = True
            status_code = 400
            url = "https://api.anthropic.com/v1/messages"
            text = '{"error":{"message":"max_tokens: 128000 exceeds the maximum"}}'

            async def aread(self) -> None:
                pass

        with pytest.raises(ProviderHTTPError, match="max_tokens: 128000 exceeds"):
            await raise_with_body(_ErrResponse())  # ty: ignore[invalid-argument-type]


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
        monkeypatch.setattr("wing.agent.core.get_config", lambda: two)

        # 跨 provider 切模型
        session._apply_model("model-2", provider_name="p2")
        assert session.agent.model_provider.name == "p2"
        # 旧 client 未被关闭（在途生成不被打断）
        assert old_provider._client.is_closed is False
        # 旧 provider 仍被 agent 有界持有（Session 不持有 provider）
        assert session.agent._providers["default"] is old_provider
        assert not hasattr(session, "_providers")

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

    @pytest.mark.asyncio
    async def test_switch_template_closes_old_agent_providers(self, sm, monkeypatch):
        """模板切换：旧 agent 的整个 provider 表被关闭，新 agent 从空表开始。

        锁定 bot#1 修复：switch_template 后不得交回已关闭的 client。
        """
        from wing.agent_template import AgentTemplate

        session = sm.create_session()
        agent_v1 = session.agent
        p_default = agent_v1.model_provider

        # 先跨 provider 用一次 p2，使旧 agent 表中含两个 client
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
        monkeypatch.setattr("wing.agent.core.get_config", lambda: two)
        monkeypatch.setattr("wing.session.get_config", lambda: two)
        session._apply_model("model-2", provider_name="p2")
        p2 = agent_v1._providers["p2"]

        # 切换模板（回到 default provider 的新 agent）
        template = AgentTemplate(name="default", model="gpt-4", provider_name="default")
        await session.switch_template(template)

        # 旧 agent 的两个 client 全部关闭
        assert p_default._client.is_closed is True
        assert p2._client.is_closed is True
        assert agent_v1._providers == {}
        # 新 agent 是另一实例，持有全新的活跃 provider
        assert session.agent is not agent_v1
        assert session.agent.model_provider.name == "default"
        assert session.agent.model_provider._client.is_closed is False

    @pytest.mark.asyncio
    async def test_rebuild_providers_evicts_all_and_recreates_active(
        self, sm, monkeypatch
    ):
        """驱逐重建：表中 client 全部关闭驱逐，活跃 provider 按新配置重建。

        锁定 bot#2 修复：api_key 轮换经 reload（驱逐重建）自然生效。
        """
        session = sm.create_session()
        agent = session.agent
        old_provider = agent.model_provider

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
        monkeypatch.setattr("wing.agent.core.get_config", lambda: two)
        # 表中放入第二个 provider（模拟跨 provider 用过）
        session._apply_model("model-2", provider_name="p2")
        p2 = agent._providers["p2"]
        session._apply_model("model-3", provider_name="default")

        # 新配置：default 的 api_key 轮换
        rotated = Config(
            providers=[
                ProviderConfig(
                    name="default", base_url="https://a.example.com", api_key="NEW-KEY"
                ),
            ],
            agents=[AgentConfig(name="default", model="gpt-4", provider="default")],
        )
        monkeypatch.setattr("wing.agent.core.get_config", lambda: rotated)

        await agent.rebuild_providers()

        # 旧 client 全部关闭、驱逐
        assert old_provider._client.is_closed is True
        assert p2._client.is_closed is True
        # 活跃 provider 按新配置重建（新实例、新 key），model 名保持
        assert agent.model_provider is not old_provider
        assert agent.model_provider._client.is_closed is False
        assert agent._providers == {"default": agent.model_provider}
        assert agent.model == "model-3"

    @pytest.mark.asyncio
    async def test_rebuild_failure_keeps_old_clients_live(self, sm, monkeypatch):
        """先建后关：重建失败时旧 client 保持可用，session 不被钉死。

        场景：新配置移除了活跃 provider 名 → get_provider 抛错；此时旧
        client 必须仍然打开、agent 状态不变（而非关了一切后重建失败）。
        """
        session = sm.create_session()
        agent = session.agent
        old_provider = agent.model_provider

        empty = Config(
            providers=[
                ProviderConfig(
                    name="other", base_url="https://b.example.com", api_key="k"
                )
            ],
            agents=[AgentConfig(name="default", model="gpt-4", provider="other")],
        )
        monkeypatch.setattr("wing.agent.core.get_config", lambda: empty)

        with pytest.raises(ValueError, match="provider 'default' not found"):
            await agent.rebuild_providers()

        # 旧 client 未关闭、活跃 provider 与表均不变
        assert old_provider._client.is_closed is False
        assert agent.model_provider is old_provider
        assert agent._providers["default"] is old_provider


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
    async def test_stream_emits_authoritative_blocks(self):
        """流结束时最终 chunk 携带权威 content_blocks（thinking→text→tool_use）。"""
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
                                        "arguments": '{"cmd":"ls"}',
                                    },
                                }
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

        blocks = None
        async for chunk in p._generate_stream(body={}, model="gpt-4"):
            if chunk.content_blocks is not None:
                blocks = chunk.content_blocks

        assert blocks is not None, "最终 chunk 必须携带权威块数组"
        assert [type(b).__name__ for b in blocks] == [
            "ThinkingBlock",
            "TextBlock",
            "ToolUseBlock",
        ]
        assert blocks[0].thinking == "let me think"  # ty: ignore[unresolved-attribute]
        assert blocks[0].signature is None  # ty: ignore[unresolved-attribute]
        assert blocks[1].text == "hello world"  # ty: ignore[unresolved-attribute]
        assert blocks[2].name == "Bash"  # ty: ignore[unresolved-attribute]
        assert blocks[2].input == {"cmd": "ls"}  # ty: ignore[unresolved-attribute]

    @pytest.mark.asyncio
    async def test_final_blocks_chunk_carries_zero_usage(self):
        """最终权威块数组 chunk 只带零 token usage 元信息。

        锁定 token 双计回归：非零 usage 已由带内 usage chunk 发射一次
        （上游 metrics 按事件累加）；最终 chunk 再附着同一非零 usage 会
        使所有 OpenAI-compat 流式调用的 token 统计翻倍。
        """
        chunks = [
            {"choices": [{"delta": {"content": "hi"}}]},
            {"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
        ]
        p = self._make()
        await p._client.aclose()
        p._client = _FakeClient(_openai_sse(chunks))  # ty: ignore[invalid-assignment]

        nonzero: list = []
        final = None
        async for chunk in p._generate_stream(body={}, model="gpt-4"):
            if chunk.content_blocks is not None:
                final = chunk
            elif chunk.usage.prompt_tokens or chunk.usage.completion_tokens:
                nonzero.append(chunk.usage)

        assert len(nonzero) == 1, "带内 usage chunk 恰好一个"
        assert final is not None
        assert final.usage.prompt_tokens == 0
        assert final.usage.completion_tokens == 0
        assert final.usage.model == "gpt-4", "元信息保留"

    def test_sync_builds_blocks(self):
        """同步路径同样产出 content_blocks。"""
        p = self._make()
        blocks = p._build_content_blocks(
            "hmm", "answer", [ToolCall(id="c1", name="Bash", arguments={"a": 1})]
        )
        assert len(blocks) == 3
        assert isinstance(blocks[0], ThinkingBlock) and blocks[0].thinking == "hmm"
        assert isinstance(blocks[1], TextBlock) and blocks[1].text == "answer"
        assert isinstance(blocks[2], ToolUseBlock) and blocks[2].input == {"a": 1}
        # 空响应 → 空块数组（合法空 turn，非 None）
        assert p._build_content_blocks(None, None, []) == []

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
