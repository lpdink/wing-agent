"""流累积契约测试——#68 中断补提交的 provider 侧回归。

锁定（两个协议实现共享同一契约）：
- 尝试期间 `accumulator.state` 指向接收增量的同一状态对象——回归：
  anthropic `_generate_stream` 中重复的 `state = _StreamState()` 覆盖了
  绑定，`snapshot_blocks()` 恒走后一个从未被写入的对象，打断后已生成
  内容既不落盘（nothing 补提交）也不回放；
- 取消后 `snapshot_blocks()` 取回已终结块；未终结 tool 调用只出现在
  `pending_tool_calls()`（互斥投影）；
- 空 thinking 块（无文本、无签名、非 redacted）不进快照；
- 端到端（真实 provider + react_loop 打断）：Anthropic 路径 partial
  assistant 落链并在下一轮请求回放 thinking；OpenAI 路径下一轮请求
  thinking-only 消息不再输出 null content。
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Callable
from typing import TYPE_CHECKING, Any

import pytest

from wing.event_bus import event_bus
from wing.provider.anthropic import AnthropicProvider, _StreamState
from wing.provider.base import ModelProvider, StreamAccumulator
from wing.provider.openai_compat import OpenAICompatProvider
from wing.schema import LLMResponse, Message, TextBlock, ThinkingBlock

if TYPE_CHECKING:
    from wing.agent import WingAgent
    from wing.runtime import WingRuntime


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    """每个测试前后清理全局 EventBus。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


# ─── SSE 伪造（可取消：喂完行后挂起，等待消费方取消） ───────────


class _HoldingResponse:
    """逐行喂 SSE；喂完后挂起保持流打开——测试在此刻取消消费。"""

    is_error = False

    def __init__(self, lines: list[str]) -> None:
        self._lines = lines
        self.headers = {"request-id": "test-rid", "x-request-id": "test-rid"}

    async def aiter_lines(self):
        for line in self._lines:
            yield line
        await asyncio.Event().wait()  # 挂起：等待被取消

    async def aclose(self) -> None:
        pass


class _FakeClient:
    """httpx.AsyncClient 替身：记录请求 body，返回可控 SSE 流。"""

    def __init__(self, lines: list[str]) -> None:
        self._lines = lines
        self.bodies: list[dict] = []

    def build_request(self, method: str, url: str, **kwargs: Any) -> object:
        body = kwargs.get("json")
        if isinstance(body, dict):
            self.bodies.append(body)
        return object()

    async def send(self, request: object, stream: bool = False) -> "_HoldingResponse":
        return _HoldingResponse(self._lines)

    async def aclose(self) -> None:
        pass


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
    return lines


def _make_anthropic() -> AnthropicProvider:
    from wing.config import ProviderConfig

    return AnthropicProvider(
        ProviderConfig(
            name="acc-anthropic",
            protocol="anthropic",
            base_url="https://api.anthropic.com",
            api_key="sk-test",
        )
    )


def _make_openai() -> OpenAICompatProvider:
    from wing.config import ProviderConfig

    return OpenAICompatProvider(
        ProviderConfig(
            name="acc-openai",
            protocol="openai",
            base_url="https://api.example.com",
            api_key="sk-test",
        )
    )


async def _consume(
    provider: ModelProvider,
    acc: StreamAccumulator,
    on_chunk: Callable[[LLMResponse], None] | None = None,
) -> None:
    """消费 provider 流直至被取消（fake 流挂起）。"""
    async for chunk in provider.generate(
        messages=[Message(role="user", content="hi")],
        model="test-model",
        stream=True,
        accumulator=acc,
    ):
        if on_chunk is not None:
            on_chunk(chunk)


async def _wait_until(pred: Callable[[], bool], timeout: float = 5.0) -> None:
    async def _inner() -> None:
        while not pred():
            await asyncio.sleep(0.005)

    await asyncio.wait_for(_inner(), timeout)


async def _cancel(task: asyncio.Task) -> None:
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task


# ─── 绑定与快照 ──────────────────────────────────────────────────


class TestAccumulatorBinding:
    @pytest.mark.asyncio
    async def test_anthropic_snapshot_reflects_consumed_deltas(self):
        """Anthropic：消费到 thinking delta 后取消，快照含已累积文本。

        回归：重复 `_StreamState()` 初始化使 accumulator 指向空对象，
        此断言此前为 None。
        """
        provider = _make_anthropic()
        try:
            await provider._client.aclose()
            provider._client = _FakeClient(  # ty: ignore[invalid-assignment]
                _anthropic_sse(
                    [
                        {
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {"type": "thinking", "thinking": ""},
                        },
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {
                                "type": "thinking_delta",
                                "thinking": "half-done reasoning",
                            },
                        },
                    ]
                )
            )
            acc = provider.create_accumulator()
            seen = asyncio.Event()
            task = asyncio.create_task(
                _consume(
                    provider,
                    acc,
                    lambda c: seen.set() if c.reasoning_content else None,
                )
            )
            await asyncio.wait_for(seen.wait(), timeout=5)
            await _cancel(task)

            blocks = provider.snapshot_blocks(acc)
            assert blocks is not None
            assert isinstance(blocks[0], ThinkingBlock)
            assert blocks[0].thinking == "half-done reasoning"
        finally:
            await provider.aclose()

    @pytest.mark.asyncio
    async def test_anthropic_retry_replaces_stale_snapshot_state(self):
        """每次尝试重置：新尝试装入新状态覆盖上次残留，快照不串味。"""
        provider = _make_anthropic()
        try:
            await provider._client.aclose()
            provider._client = _FakeClient(  # ty: ignore[invalid-assignment]
                _anthropic_sse(
                    [
                        {
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {"type": "thinking", "thinking": ""},
                        },
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {
                                "type": "thinking_delta",
                                "thinking": "half-done reasoning",
                            },
                        },
                    ]
                )
            )
            acc = provider.create_accumulator()
            stale = _StreamState()
            stale.blocks_by_index[0] = ThinkingBlock(
                thinking="stale from failed attempt"
            )
            acc.state = stale  # 上次尝试的残留（退避窗口内）

            seen = asyncio.Event()
            task = asyncio.create_task(
                _consume(
                    provider,
                    acc,
                    lambda c: seen.set() if c.reasoning_content else None,
                )
            )
            await asyncio.wait_for(seen.wait(), timeout=5)
            await _cancel(task)

            blocks = provider.snapshot_blocks(acc)
            assert blocks is not None
            assert [b.thinking for b in blocks] == ["half-done reasoning"]
        finally:
            await provider.aclose()

    @pytest.mark.asyncio
    async def test_openai_snapshot_reflects_consumed_deltas(self):
        """OpenAI：同一契约（对照，防单侧回归）。"""
        provider = _make_openai()
        try:
            await provider._client.aclose()
            provider._client = _FakeClient(  # ty: ignore[invalid-assignment]
                _openai_sse([{"choices": [{"delta": {"content": "partial answer"}}]}])
            )
            acc = provider.create_accumulator()
            seen = asyncio.Event()
            task = asyncio.create_task(
                _consume(provider, acc, lambda c: seen.set() if c.content else None)
            )
            await asyncio.wait_for(seen.wait(), timeout=5)
            await _cancel(task)

            blocks = provider.snapshot_blocks(acc)
            assert blocks is not None
            assert isinstance(blocks[0], TextBlock)
            assert blocks[0].text == "partial answer"
        finally:
            await provider.aclose()

    @pytest.mark.asyncio
    async def test_anthropic_pending_tool_visible_not_in_snapshot(self):
        """未终结 tool：pending_tool_calls 可见原始参数，快照不含它。"""
        provider = _make_anthropic()
        try:
            await provider._client.aclose()
            provider._client = _FakeClient(  # ty: ignore[invalid-assignment]
                _anthropic_sse(
                    [
                        {
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {
                                "type": "tool_use",
                                "id": "t1",
                                "name": "Bash",
                            },
                        },
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": '{"cmd":',
                            },
                        },
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": '"ls"}',
                            },
                        },
                    ]
                )
            )
            acc = provider.create_accumulator()
            task = asyncio.create_task(_consume(provider, acc))
            await _wait_until(
                lambda: bool(
                    provider.pending_tool_calls(acc)
                    and provider.pending_tool_calls(acc)[0].args_fragment
                    == '{"cmd":"ls"}'
                )
            )
            await _cancel(task)

            views = provider.pending_tool_calls(acc)
            assert [(v.tool_call_id, v.tool_name, v.args_fragment) for v in views] == [
                ("t1", "Bash", '{"cmd":"ls"}')
            ]
            # 互斥：未终结 tool 不在快照中（无任何已终结内容 → None）
            assert provider.snapshot_blocks(acc) is None
        finally:
            await provider.aclose()

    @pytest.mark.asyncio
    async def test_openai_pending_tool_visible_not_in_snapshot(self):
        """OpenAI：同一契约（对照）。"""
        provider = _make_openai()
        try:
            await provider._client.aclose()
            provider._client = _FakeClient(  # ty: ignore[invalid-assignment]
                _openai_sse(
                    [
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
                                            {
                                                "index": 0,
                                                "function": {
                                                    "arguments": '"ls"}',
                                                },
                                            }
                                        ]
                                    }
                                }
                            ]
                        },
                    ]
                )
            )
            acc = provider.create_accumulator()
            task = asyncio.create_task(_consume(provider, acc))
            await _wait_until(
                lambda: bool(
                    provider.pending_tool_calls(acc)
                    and provider.pending_tool_calls(acc)[0].args_fragment
                    == '{"cmd":"ls"}'
                )
            )
            await _cancel(task)

            views = provider.pending_tool_calls(acc)
            assert [(v.tool_call_id, v.tool_name, v.args_fragment) for v in views] == [
                ("c1", "Bash", '{"cmd":"ls"}')
            ]
            assert provider.snapshot_blocks(acc) is None
        finally:
            await provider.aclose()


# ─── 空 thinking 块剔除 ──────────────────────────────────────────


class TestEmptyThinkingBlockFilter:
    def _provider(self) -> AnthropicProvider:
        return AnthropicProvider.__new__(AnthropicProvider)

    def test_empty_thinking_excluded_keeps_other_blocks(self):
        provider = self._provider()
        state = _StreamState()
        state.blocks_by_index[0] = ThinkingBlock(thinking="", signature="")
        state.blocks_by_index[1] = TextBlock(text="answer")
        acc = provider.create_accumulator()
        acc.state = state

        blocks = provider.snapshot_blocks(acc)
        assert blocks is not None
        assert [type(b).__name__ for b in blocks] == ["TextBlock"]

    def test_only_empty_thinking_snapshot_is_none(self):
        """取消恰逢 block_start 与首个 delta 之间：不补提交空消息。"""
        provider = self._provider()
        state = _StreamState()
        state.blocks_by_index[0] = ThinkingBlock(thinking="")
        acc = provider.create_accumulator()
        acc.state = state

        assert provider.snapshot_blocks(acc) is None
        assert AnthropicProvider._ordered_finalized_blocks(state) == []

    def test_signature_and_redacted_empty_thinking_kept(self):
        """带签名 / redacted 的空文本块携带不可重建信息，必须保留。"""
        provider = self._provider()
        state = _StreamState()
        state.blocks_by_index[0] = ThinkingBlock(thinking="", signature="sig")
        state.blocks_by_index[1] = ThinkingBlock(
            thinking="", signature="blob", redacted=True
        )
        state.blocks_by_index[2] = ThinkingBlock(thinking="real reasoning")
        acc = provider.create_accumulator()
        acc.state = state

        blocks = provider.snapshot_blocks(acc)
        assert blocks is not None
        assert len(blocks) == 3


# ─── 端到端：打断补提交 + 下一轮请求 ─────────────────────────────


async def _interrupted_agent(
    runtime: "WingRuntime",
    provider: AnthropicProvider | OpenAICompatProvider,
    lines: list[str],
) -> tuple["WingAgent", _FakeClient]:
    """真实 provider 起一轮，等 delta 进入累积状态后打断。

    返回 (agent, client)：client 记录每一轮请求 body。
    """
    await provider._client.aclose()
    client = _FakeClient(lines)
    provider._client = client  # ty: ignore[invalid-assignment]

    session = runtime.create_session()
    agent = session.agent
    agent.set_model("test-model", provider)

    await agent.post("go")
    # 等到 delta 进入真实累积状态（正是打断补提交的数据源）
    await _wait_until(
        lambda: (
            agent._loop.current_acc is not None
            and bool(provider.snapshot_blocks(agent._loop.current_acc))
        )
    )
    await agent.interrupt()
    return agent, client


class TestInterruptCommitEndToEnd:
    @pytest.mark.asyncio
    async def test_anthropic_interrupt_commits_partial_and_replays_thinking(
        self, runtime
    ):
        """Anthropic：打断 → partial assistant 落链 → 下一轮回放 thinking。

        回归链：重复状态初始化 → 快照恒 None → nothing 落盘 → reasoning
        丢失（本测试在修复前超时于等待补提交数据源）。
        """
        provider = _make_anthropic()
        try:
            agent, client = await _interrupted_agent(
                runtime,
                provider,
                _anthropic_sse(
                    [
                        {
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {"type": "thinking", "thinking": ""},
                        },
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {
                                "type": "thinking_delta",
                                "thinking": "half-done reasoning",
                            },
                        },
                    ]
                ),
            )

            chain = agent.context_manager.get_context_window()
            assert [m.role for m in chain] == ["user", "assistant"]
            partial = chain[1]
            assert partial.stop_reason == "interrupted"
            assert partial.reasoning_content == "half-done reasoning"
            assert partial.content is None  # 内部 None 语义保留

            # 继续对话：下一轮 Anthropic 请求携带 thinking 块（保真回放）
            await agent.post("continue")
            await _wait_until(lambda: len(client.bodies) >= 2)
            messages = client.bodies[1]["messages"]
            assistant_blocks = next(
                m["content"] for m in messages if m["role"] == "assistant"
            )
            assert assistant_blocks == [
                {
                    "type": "thinking",
                    "thinking": "half-done reasoning",
                    "signature": "",
                }
            ]
            assert all(m.get("content") for m in messages)  # 无零块空轮

            await agent.shutdown()
        finally:
            await provider.aclose()

    @pytest.mark.asyncio
    async def test_openai_next_request_has_no_null_content(self, runtime):
        """OpenAI 兼容：thinking-only partial 的下一轮请求无 null content。

        回归（#68 + 严格网关）：打断于 thinking 阶段落链的 assistant
        （content 派生 None）序列化为 content:null，阿里云 MaaS 等网关
        400 并毒化整个会话。
        """
        provider = _make_openai()
        try:
            agent, client = await _interrupted_agent(
                runtime,
                provider,
                _openai_sse(
                    [
                        {
                            "choices": [
                                {"delta": {"reasoning_content": "half-done reasoning"}}
                            ]
                        }
                    ]
                ),
            )

            chain = agent.context_manager.get_context_window()
            assert [m.role for m in chain] == ["user", "assistant"]
            assert chain[1].stop_reason == "interrupted"
            assert chain[1].reasoning_content == "half-done reasoning"

            await agent.post("continue")
            await _wait_until(lambda: len(client.bodies) >= 2)
            messages = client.bodies[1]["messages"]
            # 全量请求体不得出现 null content（json 级断言，贴近网关视角）
            assert '"content": null' not in json.dumps(messages, ensure_ascii=False)
            partial_msg = next(
                m
                for m in messages
                if m.get("reasoning_content") == "half-done reasoning"
            )
            assert partial_msg["content"] == ""
            assert partial_msg["role"] == "assistant"

            await agent.shutdown()
        finally:
            await provider.aclose()
