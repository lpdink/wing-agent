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
from wing.provider.anthropic.provider import AnthropicProvider
from wing.provider.base import RequestOptions
from wing.provider.openai.provider import OpenAICompatProvider
from wing.provider.pool import get_provider, reset_providers
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

        零块来源：存量历史的空 assistant / 旧版本 clear_reasoning 剥离
        thinking 的产物（该开关已随本修复移除，不再产生新记录）。丢弃是
        配对安全的：零块即无 tool_use，不会有后续 tool_result 引用本条。
        """
        p = _make_anthropic()
        try:
            ghost = Message(role="assistant", content_blocks=None)

            _, am = p._serialize_messages(
                [
                    Message(role="user", content="q1"),
                    ghost,
                    Message(role="user", content="q2"),
                ],
                "claude-x",
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
                ],
                "claude-x",
            )
            assert [m["role"] for m in am2] == ["user", "assistant", "user"]
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_consecutive_assistants_merged_from_invalid_round_retry(self):
        """无效轮重试续跑产生的相邻 assistant（content 条 + tool_use 条）合并为一条。

        产生路径：截断轮 content 提交后重试，重试轮补发 tool call——两条相邻
        assistant 若原样发出会违反 Anthropic 严格交替（连续同角色 400）。
        合并只 extend 块数组，恰好还原「text + tool_use 同一条」的未截断形态。
        """
        p = _make_anthropic()
        try:
            _, am = p._serialize_messages(
                [
                    Message(role="user", content="go"),
                    Message(role="assistant", content="let me check"),
                    Message(
                        role="assistant",
                        content_blocks=[
                            ToolUseBlock(id="t1", name="Bash", input={"cmd": "ls"})
                        ],
                    ),
                    Message(role="tool", tool_call_id="t1", content="ok"),
                ],
                "claude-x",
            )
            # 两条 assistant 合并；tool_result 跟在合并后的 assistant 之后。
            assert [m["role"] for m in am] == ["user", "assistant", "user"]
            blocks = am[1]["content"]
            assert [b["type"] for b in blocks] == ["text", "tool_use"]
            assert blocks[0]["text"] == "let me check"
            assert blocks[1]["id"] == "t1"
            assert am[2]["content"][0]["tool_use_id"] == "t1"
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
    async def test_malformed_tool_input_sets_input_error_not_raises(self):
        """tool input JSON 非法 → 流正常完成，块带 input_error、
        ToolCall 带 arguments_error（与 OpenAI 路径同构，MUST NOT 抛异常）。"""
        raw = '{"questions": [{"id": "q1",},]}'
        events = [
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "t0",
                    "name": "AskUserQuestion",
                },
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": raw},
            },
            {"type": "content_block_stop", "index": 0},
            {"type": "message_stop"},
        ]
        p = _make_anthropic()
        await p._client.aclose()
        p._client = _FakeClient(_anthropic_sse(events))  # ty: ignore[invalid-assignment]

        final_tc = None
        blocks = None
        async for chunk in p._generate_stream(body={}, model="claude"):
            for tc in chunk.tool_calls or []:
                final_tc = tc
            if chunk.content_blocks is not None:
                blocks = chunk.content_blocks

        assert final_tc is not None
        assert final_tc.arguments == {}
        assert final_tc.arguments_error is not None
        assert raw in final_tc.arguments_error
        assert blocks is not None and len(blocks) == 1
        assert blocks[0].input == {}  # ty: ignore[unresolved-attribute]
        assert blocks[0].input_error is not None  # ty: ignore[unresolved-attribute]

    @pytest.mark.asyncio
    async def test_stream_error_event_raises(self):
        """流中 error 事件抛 ProviderStreamError（触发重试，截断轮次不提交）。"""
        from wing.provider.transport import ProviderStreamError

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
        from wing.provider.transport import ProviderHTTPError, raise_with_body

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
    async def test_interleaved_beta_header_follows_thinking_state(self):
        """beta header 每请求计算，跟随会话级 thinking 覆盖（不固化在 client）。

        缺 interleaved beta header 则工具轮次间不会产生多块 thinking，
        故 options.thinking 覆盖后 header 必须跟随；无覆盖 = 配置基线。
        """
        enabled = _make_anthropic({"thinking": {"type": "enabled"}})
        disabled = _make_anthropic({"thinking": {"type": "disabled"}})
        try:
            assert enabled._request_headers() == {
                "anthropic-beta": "interleaved-thinking-2025-05-14"
            }
            # 覆盖关闭 → 下次请求 header 跟随（配置基线不受影响）
            assert enabled._request_headers(RequestOptions(thinking=False)) == {}
            assert "anthropic-beta" in enabled._request_headers(
                RequestOptions(thinking=True)
            )
            # 配置禁用 + 覆盖启用 → 跟随恢复
            assert disabled._request_headers() == {}
            assert "anthropic-beta" in disabled._request_headers(
                RequestOptions(thinking=True)
            )
        finally:
            await enabled.aclose()
            await disabled.aclose()

    @pytest.mark.asyncio
    async def test_static_headers_have_no_beta(self):
        """静态 header（client 固化）不含 beta——beta 只走每请求动态路径。"""
        headers = AnthropicProvider._make_headers("sk", "2023-06-01")
        assert "anthropic-beta" not in headers
        assert headers["x-api-key"] == "sk"

    @pytest.mark.asyncio
    async def test_thinking_override_roundtrip_self_consistent(self):
        """会话级 thinking 覆盖（RequestOptions）的往返自洽：请求体跟随覆盖，
        共享 extra_body（配置基线）零改写——覆盖只活在 per-request 视图里。"""
        p = _make_anthropic({"thinking": {"type": "enabled", "budget_tokens": 8192}})
        try:
            msgs = [Message(role="user", content="hi")]

            # 覆盖关闭：body thinking.type=disabled，且按校验剥离 budget
            body = p._build_body(
                msgs, "claude-x", None, False, RequestOptions(thinking=False)
            )
            assert body["thinking"] == {"type": "disabled"}
            # 共享状态保留用户原预算；配置基线不变（无覆盖时仍启用）
            assert p._extra_body["thinking"]["budget_tokens"] == 8192
            assert p.thinking is True

            # 覆盖启用：用户原预算原样恢复（非默认值）
            body = p._build_body(
                msgs, "claude-x", None, False, RequestOptions(thinking=True)
            )
            assert body["thinking"] == {"type": "enabled", "budget_tokens": 8192}
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_thinking_override_does_not_mutate_provider_config(self):
        """会话级覆盖不得写穿共享配置（ProviderConfig / 实例 extra_body 皆只读）。

        嵌套的 thinking dict 若被覆盖逻辑原地改写，会污染同进程其他会话
        与池中其他引用者（review 发现：浅拷贝 + setdefault 的历史坑）。
        """
        cfg = ProviderConfig(
            name="test-anthropic",
            protocol="anthropic",
            base_url="https://api.anthropic.com",
            api_key="sk-test",
            extra_body={"thinking": {"type": "disabled"}},
        )
        p = AnthropicProvider(cfg)
        try:
            msgs = [Message(role="user", content="hi")]
            body = p._build_body(
                msgs, "claude-x", None, False, RequestOptions(thinking=True)
            )
            assert body["thinking"]["type"] == "enabled"
            # 覆盖不落共享状态：配置基线 / 实例视图均保持原样（budget 不渗入）
            assert p.thinking is False
            assert cfg.extra_body == {"thinking": {"type": "disabled"}}
            assert p._extra_body == {"thinking": {"type": "disabled"}}
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_thinking_override_enable_fills_default_budget(self):
        """无 thinking 配置时覆盖启用：补默认预算（type=enabled 必带 budget）。"""
        from wing.provider.anthropic.provider import _DEFAULT_THINKING_BUDGET

        p = _make_anthropic({})
        try:
            body = p._build_body(
                [Message(role="user", content="hi")],
                "m",
                None,
                False,
                RequestOptions(thinking=True),
            )
            assert body["thinking"] == {
                "type": "enabled",
                "budget_tokens": _DEFAULT_THINKING_BUDGET,
            }
            # 共享状态不被覆盖污染
            assert p._extra_body == {}
        finally:
            await p.aclose()


# ─── Provider 生命周期 ───────────────────────────────────────────


class TestProviderLifecycle:
    """共享池的所有权语义（provider 无状态化后）。

    旧模型（每 agent 一份 client 表、切模板 / 逐出 / reload 时关闭）已废弃：
    #172 的根因正是「重建即关闭」把在途重试钉死在退场实例上。新语义：

    - 实例归全局池（``wing.provider.pool``），agent 只持有 name；
    - 同名的全部会话共享同一个实例（FD 不再按会话 × provider 放大）；
    - 切 provider / 切模板 / 逐出不关闭任何 client（其他会话可能正在用）；
    - reload = 池 reset：新配置建新实例替换，旧实例 retire（无在途即刻关闭，
      有在途则等收尾——在途请求绝不被打断）。
    """

    @pytest.fixture
    def sm(self):
        from wing.session import SessionManager
        from wing.store import MemorySessionStore

        return SessionManager(
            {"memory": MemorySessionStore()}, default_backend="memory"
        )

    @staticmethod
    def _use_config(monkeypatch: pytest.MonkeyPatch, cfg: Config) -> None:
        """让所有读取方都看到 cfg。

        池在调用点惰性取 ``wing.config.get_config``（名字 patch 即覆盖）；
        其余模块（session / agent / CM）持有的是原始函数引用，读
        ``loader._config`` 单例——两处都要指到同一份。
        """
        monkeypatch.setattr("wing.config.get_config", lambda: cfg)
        monkeypatch.setattr("wing.config.loader._config", cfg)

    @staticmethod
    def _two_provider_config(api_key: str = "k") -> Config:
        return Config(
            providers=[
                ProviderConfig(
                    name="default",
                    base_url="https://a.example.com",
                    api_key=api_key,
                    models=["gpt-4"],
                ),
                ProviderConfig(
                    name="p2",
                    base_url="https://b.example.com",
                    api_key="k",
                    models=["model-2", "model-3"],
                ),
            ],
            agents=[AgentConfig(name="default", model="gpt-4")],
        )

    @pytest.mark.asyncio
    async def test_same_provider_name_shares_one_instance_across_sessions(self, sm):
        """同名 provider 在全进程只有一个实例：多会话共享（FD 不再按会话放大）。"""
        s1 = sm.create_session()
        s2 = sm.create_session()
        assert s1.agent.model_provider is s2.agent.model_provider
        assert s1.agent.model_provider is get_provider("default")

    @pytest.mark.asyncio
    async def test_cross_provider_switch_resolves_from_pool(self, sm, monkeypatch):
        """跨 provider 切模型（按 id）：实例经池解析；切回同名复用同一实例。"""
        self._use_config(monkeypatch, self._two_provider_config())
        session = sm.create_session()
        p_default = session.agent.model_provider
        assert p_default.name == "default"

        session._apply_model("model-2")  # p2 声明的 id
        assert session.agent.model_provider.name == "p2"
        assert session.agent.model_provider is not p_default
        # 旧实例仍在池中可用（其他会话可能钉着它），未被关闭
        assert p_default._client.is_closed is False
        assert get_provider("default") is p_default

        # 切回 default 声明的 id：复用池中同一实例，而非新建
        session._apply_model("gpt-4")
        assert session.agent.model_provider is p_default

    @pytest.mark.asyncio
    async def test_switch_template_keeps_shared_clients_open(self, sm):
        """模板切换不关闭任何 client：实例归池，新 agent 解析到同一共享实例。"""
        from wing.session import AgentTemplate

        session = sm.create_session()
        p_default = session.agent.model_provider
        agent_v1 = session.agent

        template = AgentTemplate(name="default", model="gpt-4", provider_name="default")
        await session.switch_template(template)

        assert p_default._client.is_closed is False
        assert session.agent is not agent_v1
        assert session.agent.model_provider is p_default

    @pytest.mark.asyncio
    async def test_reload_resets_pool_and_retires_idle_instances(self, sm, monkeypatch):
        """reload（池 reset）：按新配置建新实例替换；无在途的旧实例即刻退场关闭。"""
        session = sm.create_session()
        old = session.agent.model_provider

        self._use_config(monkeypatch, self._two_provider_config(api_key="NEW-KEY"))
        rebuilt = await reset_providers()

        assert rebuilt == 2  # default + p2
        new = session.agent.model_provider
        assert new is not old
        assert old._client.is_closed is True  # 无在途 → 退场即关闭
        assert new._client.is_closed is False
        assert new._config.api_key == "NEW-KEY"

    @pytest.mark.asyncio
    async def test_reload_failure_keeps_pool_intact(self, sm, monkeypatch):
        """先建后换：任一 provider 构建失败时池保持原样（会话不被钉死）。"""
        session = sm.create_session()
        old = session.agent.model_provider

        import wing.provider.pool as pool_mod

        def _boom(cfg):
            raise ValueError(f"unsupported protocol: '{cfg.protocol}'")

        monkeypatch.setattr(pool_mod, "create_provider", _boom)
        with pytest.raises(ValueError, match="unsupported protocol"):
            await reset_providers()

        # 池未被改动：旧 client 未关闭、活跃 provider 不变
        assert old._client.is_closed is False
        assert session.agent.model_provider is old

    @pytest.mark.asyncio
    async def test_removed_provider_stays_usable_for_pinned_sessions(
        self, sm, monkeypatch
    ):
        """配置移除某 provider：池保留旧实例——钉在它上面的会话不被 reload 拆解。"""
        self._use_config(monkeypatch, self._two_provider_config())
        session = sm.create_session()
        session._apply_model("model-2")  # p2 声明的 id
        p2 = session.agent.model_provider

        # 新配置只剩 default（p2 被移除）
        only_default = Config(
            providers=[
                ProviderConfig(
                    name="default",
                    base_url="https://a.example.com",
                    api_key="k",
                    models=["gpt-4"],
                ),
            ],
            agents=[AgentConfig(name="default", model="gpt-4")],
        )
        self._use_config(monkeypatch, only_default)
        await reset_providers()

        assert p2._client.is_closed is False
        assert session.agent.model_provider is p2


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

    @pytest.mark.asyncio
    async def test_thinking_defaults_sent_in_body(self):
        """基线行为：enable_thinking / preserve_thinking 默认随请求发送。

        preserve_thinking 尤为关键——缺它则多轮工具回合间 thinking 被服务端剥离。
        """
        p = self._make()
        try:
            assert p.thinking is True
            body = p._build_body(
                [Message(role="user", content="hi")], "gpt-4", None, False
            )
            assert body["enable_thinking"] is True
            assert body["preserve_thinking"] is True
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_thinking_override_flips_body_only(self):
        """options.thinking 覆盖：请求体跟随翻转；共享状态（配置基线）零改写。"""
        p = self._make()
        try:
            msgs = [Message(role="user", content="hi")]
            body = p._build_body(
                msgs, "gpt-4", None, False, RequestOptions(thinking=False)
            )
            assert body["enable_thinking"] is False
            # preserve_thinking 不随开关变化（与 develop 基线一致：恒真）
            assert body["preserve_thinking"] is True
            assert p.thinking is True  # 无覆盖时的基线不受影响

            body = p._build_body(
                msgs, "gpt-4", None, False, RequestOptions(thinking=True)
            )
            assert body["enable_thinking"] is True
        finally:
            await p.aclose()

    @pytest.mark.asyncio
    async def test_user_extra_body_overrides_defaults(self):
        """用户 extra_body 显式值优先于内置默认。"""
        cfg = ProviderConfig(
            name="test-openai",
            protocol="openai",
            base_url="https://api.example.com",
            api_key="sk-test",
            extra_body={"enable_thinking": False, "preserve_thinking": False},
        )
        p = OpenAICompatProvider(cfg)
        try:
            assert p.thinking is False
            body = p._build_body(
                [Message(role="user", content="hi")], "gpt-4", None, False
            )
            assert body["enable_thinking"] is False
            assert body["preserve_thinking"] is False
        finally:
            await p.aclose()
