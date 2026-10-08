"""无效轮次的判定与自动重试（`ReActLoop._call_llm_validated`）。

规则（上游「空响应 / 流被截断」的容错）：

1. 有任一收敛（已终结）的 tool call → 绝不重试（自然进入下一轮）；
2. 无收敛 tool call 且无 content（reasoning 不算）→ 无效，不提交，重试；
3. 有 content 但 tool call 起了头全未收敛 → content 提交、tool call 不提交，
   然后重试；
4. 有 content 且无 tool call 尝试 → 正常收尾。

重试经 `with_retry(retry_on=(InvalidGenerationError,))`：只重试该语义错误
（非语义异常直通——不与 provider 层传输重试叠加放大）。
"""

from __future__ import annotations

from dataclasses import dataclass
from types import SimpleNamespace
from typing import Any

import pytest

from wing.agent.event_sink import AgentEventSink
from wing.agent.react_loop import InvalidGenerationError, ReActLoop
from wing.event import NoticeEvent
from wing.event_bus import event_bus
from wing.provider.base import PendingToolView, RequestOptions, StreamAccumulator
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    TextBlock,
    ThinkingBlock,
    ToolUseBlock,
)

#: 重试参数注入：0 退避、小次数（避免真实等待）；生产默认口径为
#: 10 次、3s 起步指数退避、封顶 180s（见 ReActLoop._config 注释）。
_NO_WAIT = SimpleNamespace(max_retries=2, max_retry_delay=0.0)


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


class _Ctx:
    """_TurnAccumulator 桩：记录 usage 记账调用。"""

    def __init__(self) -> None:
        self.usage_calls: list = []

    def record_usage(self, usage: Any) -> None:
        self.usage_calls.append(usage)


class _FakeCM:
    """最小 ContextManager 桩：记录提交，固定返回空消息列表。"""

    def __init__(self) -> None:
        self.added: list[list[Message]] = []

    async def get_messages_for_llm(self, **_: Any) -> SimpleNamespace:
        return SimpleNamespace(messages=[], tools=[])

    def add_messages(self, messages: list[Message]) -> None:
        self.added.append(list(messages))


@dataclass
class _FakeState:
    """假累积状态：unfinished = 流结束时残留的未终结 tool call 数。"""

    unfinished: int = 0


class _FakeProvider:
    """剧本化 provider：每次 generate 消费一个 attempt。

    attempt = {"chunks": [...], "unfinished": n}；unfinished 模拟流结束时
    残留的未终结 tool call 数（截断检测的数据源）。
    """

    def __init__(self, attempts: list[dict], *, raise_exc: Exception | None = None):
        self._attempts = list(attempts)
        self._raise = raise_exc
        self.calls = 0

    async def generate(self, *, accumulator: Any = None, **_: Any):
        self.calls += 1
        if self._raise is not None:
            raise self._raise
        spec = self._attempts.pop(0)
        if accumulator is not None:
            accumulator.state = _FakeState(unfinished=spec.get("unfinished", 0))
        for chunk in spec["chunks"]:
            yield chunk

    def create_accumulator(self) -> StreamAccumulator:
        return StreamAccumulator()

    def snapshot_blocks(self, accumulator: Any) -> None:
        return None

    def pending_tool_calls(self, accumulator: StreamAccumulator) -> list:
        state = accumulator.state if accumulator is not None else None
        count = state.unfinished if isinstance(state, _FakeState) else 0
        return [
            PendingToolView(tool_call_id=f"u{i}", tool_name="Read", args_fragment="{")
            for i in range(count)
        ]

    def unfinished_tool_calls(self, accumulator: StreamAccumulator) -> int:
        state = accumulator.state if accumulator is not None else None
        return state.unfinished if isinstance(state, _FakeState) else 0


def _make_loop(cm: _FakeCM, provider: _FakeProvider) -> ReActLoop:
    loop = ReActLoop(
        tool_executor=None,  # ty: ignore[invalid-argument-type]  # 本文件不触碰
        sink=AgentEventSink(session_id="test"),
        context_manager=cm,  # ty: ignore[invalid-argument-type]
        inbox=None,  # ty: ignore[invalid-argument-type]
        current_model=lambda: "test-model",
        current_provider=lambda: provider,  # ty: ignore[invalid-argument-type]
        current_tools=lambda: [],
        current_options=lambda: RequestOptions(),
        stream=True,
    )
    loop._config = _NO_WAIT
    return loop


def _attempt(chunks: list[LLMResponse], unfinished: int = 0) -> dict:
    return {"chunks": chunks, "unfinished": unfinished}


def _empty() -> dict:
    return _attempt([LLMResponse(content_blocks=[])])


def _text(content: str) -> dict:
    return _attempt(
        [
            LLMResponse(content=content),
            LLMResponse(content_blocks=[TextBlock(text=content)]),
        ]
    )


def _thinking_only() -> dict:
    return _attempt(
        [
            LLMResponse(reasoning_content="hmm"),
            LLMResponse(content_blocks=[ThinkingBlock(thinking="hmm")]),
        ]
    )


def _tool_call() -> dict:
    return _attempt(
        [LLMResponse(content_blocks=[ToolUseBlock(id="t1", name="Bash", input={})])]
    )


def _notices(events: list) -> list[NoticeEvent]:
    return [e for e in events if isinstance(e, NoticeEvent)]


class TestInvalidGenerationRetry:
    @pytest.mark.asyncio
    async def test_empty_retries_then_succeeds(self):
        """全空轮：无 content 且无收敛 tool call → 无效 → 重试 → 第二尝试正常返回；空轮不落链。"""
        events: list = []
        event_bus.subscribe(events.append)
        cm = _FakeCM()
        provider = _FakeProvider([_empty(), _text("hi")])
        loop = _make_loop(cm, provider)

        msg = await loop._call_llm_validated(_Ctx(), model="m")

        assert msg.content == "hi"
        assert provider.calls == 2
        assert cm.added == []  # 空轮不提交任何消息
        notices = _notices(events)
        assert len(notices) == 1
        assert notices[0].attempt == 1
        assert notices[0].max_attempts == 2
        assert notices[0].level == "warning"

    @pytest.mark.asyncio
    async def test_reasoning_only_is_invalid(self):
        """只有 reasoning：不作为收尾依据 → 无效重试。"""
        cm = _FakeCM()
        provider = _FakeProvider([_thinking_only(), _text("hi")])
        loop = _make_loop(cm, provider)

        msg = await loop._call_llm_validated(_Ctx(), model="m")

        assert msg.content == "hi"
        assert provider.calls == 2
        assert cm.added == []  # thinking 不提交

    @pytest.mark.asyncio
    async def test_converged_tool_call_never_retries(self):
        """规则 1：有任一收敛 tool call → 有效，即使同时存在未收敛调用。"""
        cm = _FakeCM()
        provider = _FakeProvider([_attempt(_tool_call()["chunks"], unfinished=1)])
        loop = _make_loop(cm, provider)

        msg = await loop._call_llm_validated(_Ctx(), model="m")

        assert msg.tool_calls is not None and msg.tool_calls[0].name == "Bash"
        assert provider.calls == 1
        assert cm.added == []

    @pytest.mark.asyncio
    async def test_content_with_truncated_tool_call_commits_content_then_retries(
        self,
    ):
        """规则 3：content 提交（tool call 不提交）→ 重试；被丢弃尝试的 usage 计入 turn 账。"""
        cm = _FakeCM()
        provider = _FakeProvider(
            [
                _attempt(
                    [
                        LLMResponse(
                            content="pre",
                            usage=LLMUsage(prompt_tokens=7, completion_tokens=3),
                        ),
                        LLMResponse(content_blocks=[TextBlock(text="pre")]),
                    ],
                    unfinished=1,
                ),
                _text("done"),
            ]
        )
        loop = _make_loop(cm, provider)
        ctx = _Ctx()

        msg = await loop._call_llm_validated(ctx, model="m")

        assert msg.content == "done"
        assert provider.calls == 2
        assert len(cm.added) == 1
        committed = cm.added[0][0]
        assert committed.role == "assistant"
        assert committed.content == "pre"
        assert committed.tool_calls is None  # 未收敛的 tool call 不提交
        # 被丢弃尝试的 usage 照常记账（token 真花掉了）
        assert [u.prompt_tokens for u in ctx.usage_calls if u] == [7]

    @pytest.mark.asyncio
    async def test_content_without_tool_attempt_ends_normally(self):
        """规则 4：有 content、无 tool call 尝试 → 正常收尾，不重试。"""
        cm = _FakeCM()
        provider = _FakeProvider([_text("final")])
        loop = _make_loop(cm, provider)

        msg = await loop._call_llm_validated(_Ctx(), model="m")

        assert msg.content == "final"
        assert provider.calls == 1
        assert cm.added == []

    @pytest.mark.asyncio
    async def test_exhausted_retries_raise(self):
        """重试耗尽：InvalidGenerationError 抛出（由 run_turn 错误路径上报）。"""
        cm = _FakeCM()
        provider = _FakeProvider([_empty(), _empty()])
        loop = _make_loop(cm, provider)
        loop._config = SimpleNamespace(max_retries=1, max_retry_delay=0.0)

        with pytest.raises(InvalidGenerationError):
            await loop._call_llm_validated(_Ctx(), model="m")

        assert provider.calls == 2  # 1 次 + 1 次重试
        assert cm.added == []

    @pytest.mark.asyncio
    async def test_non_semantic_exception_not_retried(self):
        """非语义异常直通（retry_on 过滤）——不与 provider 传输重试叠加。"""
        cm = _FakeCM()
        provider = _FakeProvider([], raise_exc=RuntimeError("boom"))
        loop = _make_loop(cm, provider)

        with pytest.raises(RuntimeError, match="boom"):
            await loop._call_llm_validated(_Ctx(), model="m")

        assert provider.calls == 1

    @pytest.mark.asyncio
    async def test_stream_without_authoritative_blocks_is_retried(self):
        """流未正常结束（无权威块数组，Anthropic 在 message_stop 前被切断）→ 同判无效并重试。"""
        cm = _FakeCM()
        provider = _FakeProvider([_attempt([]), _text("hi")])
        loop = _make_loop(cm, provider)

        msg = await loop._call_llm_validated(_Ctx(), model="m")

        assert msg.content == "hi"
        assert provider.calls == 2
        assert cm.added == []

    @pytest.mark.asyncio
    async def test_retry_re_resolves_provider_after_swap(self):
        """每次 attempt 重新解析当前 provider：重试期间 provider 被换新（等价
        reload / 池换新），第二次 attempt 必须落在新实例上并正常完成。

        #172 的根因回归：重试栈绑定 turn 开始时捕获的实例——换新后每次
        attempt 都打在退场实例上，永远不可能成功。
        """
        cm = _FakeCM()
        second = _FakeProvider([_text("recovered")])
        holder = SimpleNamespace(provider=None)

        async def swap_then_empty(*, accumulator: Any = None, **_: Any):
            holder.provider = second  # 模拟 attempt 1 期间发生的池换新
            yield LLMResponse(content_blocks=[])

        holder.provider = SimpleNamespace(
            generate=swap_then_empty, create_accumulator=StreamAccumulator
        )
        loop = _make_loop(cm, None)  # ty: ignore[invalid-argument-type]
        loop._current_provider = lambda: holder.provider

        msg = await loop._call_llm_validated(_Ctx(), model="m")

        assert msg.content == "recovered"
        assert second.calls == 1
