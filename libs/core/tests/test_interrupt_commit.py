"""打断对账测试（interrupted-turn commit）。

工具执行期间被打断（worker 被 cancel）时，exec_tool_calls 收拢每个 call
的最终结果——已完成的取真结果，被取消的合成一句话打断结果——并向
_llm_turn 抛 _InterruptedToolResults；_llm_turn 沿正常路径提交本轮消息
后再重新抛出 CancelledError 让 worker 终止。保证：

  - 不变量：context 中每个 tool_call 都有对应的 tool 消息，下次 LLM 请求
    结构合法（无悬空 tool_calls）；
  - 前端收到与入库内容一致的关 cell 事件（ToolCallResultEvent /
    ToolResultTurnEvent），TUI 的 Bash 计时器随之冻结；
  - 补提交经 MessageLog 持久化，resume 后可见。

流式三段（reasoning / content / tool 参数流式）中被打断时：半截
reasoning/text 经 provider accumulator 快照组装 partial assistant Message
提交（stop_reason="interrupted"），半截 tool call（未终结参数流）丢弃；
补提交后当前 accumulator 置空、未提交投影失效——用户可放心打断长思考，
已花费 tokens 的内容不丢。
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from wing.agent import Inbound, WingAgent
from wing.agent.tool_executor import INTERRUPTED_RESULT as _INTERRUPTED_RESULT
from wing.event import (
    AskEvent,
    DoneEvent,
    ToolCallResultEvent,
    ToolResultTurnEvent,
)
from wing.event_bus import event_bus
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    TextBlock,
    ThinkingBlock,
    Tool,
    ToolCall,
    ToolParam,
    ToolUseBlock,
)


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


def _make_tool(name: str, fn) -> Tool:
    return Tool(
        name=name,
        description=f"Test tool: {name}",
        params=[ToolParam(name="input", type="string", description="input")],
        function=fn,
    )


def _tc(tool_id: str, name: str) -> ToolCall:
    return ToolCall(id=tool_id, name=name, arguments={"input": "x"})


def _final_resp(
    content: str | None = None, tool_calls: list[ToolCall] | None = None
) -> LLMResponse:
    """模拟 provider 的最终 chunk：扁平字段 + 权威 content_blocks（新契约）。"""
    blocks = []
    if content:
        blocks.append(TextBlock(text=content))
    for tc in tool_calls or []:
        blocks.append(ToolUseBlock(id=tc.id, name=tc.name, input=tc.arguments))
    return LLMResponse(
        content=content,
        tool_calls=tool_calls,
        content_blocks=blocks,
        usage=_usage(),
    )


def _usage() -> LLMUsage:
    return LLMUsage(prompt_tokens=10, completion_tokens=5)


async def _wait_until(pred, timeout: float = 5.0) -> None:
    """轮询等待 pred() 为真。"""

    async def _inner():
        while not pred():
            await asyncio.sleep(0.005)

    await asyncio.wait_for(_inner(), timeout)


def _start_turn(agent: WingAgent, content: str = "go") -> None:
    agent._inbox._queue.put_nowait(
        Inbound(message=Message(role="user", content=content))
    )


def _result_emitted(events: list[Any], tool_call_id: str) -> bool:
    return any(
        isinstance(e, ToolCallResultEvent) and e.tool_call_id == tool_call_id
        for e in events
    )


def _assert_well_formed(messages: list[Message]) -> None:
    """不变量：带 tool_calls 的 assistant 消息后紧跟齐配的 tool 消息。"""
    for i, m in enumerate(messages):
        if m.role != "assistant" or not m.tool_calls:
            continue
        following = messages[i + 1 : i + 1 + len(m.tool_calls)]
        assert [t.role for t in following] == ["tool"] * len(m.tool_calls), (
            f"assistant tool_calls not followed by tool messages at index {i}"
        )
        assert {t.tool_call_id for t in following} == {tc.id for tc in m.tool_calls}, (
            "tool message ids do not match tool_calls"
        )


class TestInterruptDuringToolExec:
    """工具执行期间打断：补提交 assistant + 真结果 + 合成结果。"""

    @pytest.mark.asyncio
    async def test_commits_real_and_synthesized_results(self, runtime, monkeypatch):
        session = runtime.create_session()
        agent = session.agent

        slow_started = asyncio.Event()

        async def fast_tool(input: str = "") -> str:
            return "fast-result"

        async def slow_tool(input: str = "") -> str:
            slow_started.set()
            await asyncio.sleep(30)
            return "never"

        agent._executor._tools = {
            "Fast": _make_tool("Fast", fast_tool),
            "Slow": _make_tool("Slow", slow_tool),
        }

        events: list[Any] = []
        event_bus.subscribe(events.append)

        calls = [_tc("call-fast", "Fast"), _tc("call-slow", "Slow")]

        async def _generate(*args: Any, **kwargs: Any):
            yield _final_resp(tool_calls=calls)

        monkeypatch.setattr(agent.model_provider, "generate", _generate)
        _start_turn(agent)
        # 等到 fast 已完成（结果事件已发）、slow 已开跑——打断窗口正中。
        await _wait_until(
            lambda: slow_started.is_set() and _result_emitted(events, "call-fast")
        )
        await agent.interrupt()
        # interrupt() 已 await 旧 worker 完成补提交，结果事件必已发出。
        await _wait_until(lambda: _result_emitted(events, "call-slow"))

        # ── 上下文：[user, assistant(tool_calls), tool×2] ──
        chain = agent.context_manager.get_context_window()
        assert chain[0].role == "user"
        assert chain[1].role == "assistant"
        assert chain[1].tool_calls is not None
        assert len(chain[1].tool_calls) == 2
        tools = {m.tool_call_id: m.content for m in chain[2:] if m.role == "tool"}
        assert tools == {
            "call-fast": "fast-result",
            "call-slow": _INTERRUPTED_RESULT,
        }
        _assert_well_formed(chain)

        # ── 事件：仅为合成的 call 补发关 cell 事件 ──
        result_events = [e for e in events if isinstance(e, ToolCallResultEvent)]
        slow_results = [e for e in result_events if e.tool_call_id == "call-slow"]
        assert len(slow_results) == 1
        assert slow_results[0].tool_success is False
        assert slow_results[0].tool_result == _INTERRUPTED_RESULT
        # fast 的结果事件在完成时已发（success=True），不重复补发
        fast_results = [e for e in result_events if e.tool_call_id == "call-fast"]
        assert len(fast_results) == 1
        assert fast_results[0].tool_success is True

        turn_events = [
            e
            for e in events
            if isinstance(e, ToolResultTurnEvent) and e.tool_use_id == "call-slow"
        ]
        assert len(turn_events) == 1
        assert turn_events[0].is_error is True
        assert turn_events[0].content == _INTERRUPTED_RESULT

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupt_during_feedback_wait(self, runtime, monkeypatch):
        """工具阻塞在 ask_feedback 上时打断：同样合成结果入库，waiter 清空。"""
        session = runtime.create_session()
        agent = session.agent

        async def asking_tool(input: str = "") -> str:
            await agent.ask_feedback(
                AskEvent(question="proceed?", choices=["y", "n"], required=True),
                timeout=30,
            )
            return "never"

        agent._executor._tools = {"Ask": _make_tool("Ask", asking_tool)}

        events: list[Any] = []
        event_bus.subscribe(events.append)

        async def _generate(*args: Any, **kwargs: Any):
            yield _final_resp(tool_calls=[_tc("call-ask", "Ask")])

        monkeypatch.setattr(agent.model_provider, "generate", _generate)
        _start_turn(agent)
        await _wait_until(lambda: "call-ask" in agent._inbox._feedback_waiters)
        await agent.interrupt()
        await _wait_until(lambda: _result_emitted(events, "call-ask"))

        chain = agent.context_manager.get_context_window()
        tools = {m.tool_call_id: m.content for m in chain if m.role == "tool"}
        assert tools == {"call-ask": _INTERRUPTED_RESULT}
        _assert_well_formed(chain)
        assert not agent._inbox._feedback_waiters

        await agent.shutdown()


class TestInterruptDuringStreaming:
    """流式生成期间打断：半截 reasoning/text 补提交，半截 tool call 丢弃。"""

    @pytest.mark.asyncio
    async def test_interrupt_during_streaming_commits_partial(
        self, runtime, monkeypatch
    ):
        """reasoning 流式中途打断 → partial assistant Message 提交。

        - 已生成的 thinking/text 块保留（任意长度皆可提交）；
        - 未终结的 tool 参数流丢弃（无配对结果的 tool_use 不产生）；
        - stop_reason="interrupted"（截断审计）；
        - 未提交投影失效（accumulator 置空，内容已由 Message 承载）；
        - InterruptedEvent 落盘于 partial Message 之后（链序）。
        """
        session = runtime.create_session()
        agent = session.agent

        streaming = asyncio.Event()
        # snapshot 语义与真实 provider 一致：text/thinking 保留，
        # 未终结 tool 块已在 provider 侧剔除
        blocks = [
            ThinkingBlock(thinking="half-done reasoning"),
            TextBlock(text="partial answer"),
        ]

        async def _blocking_generate(*args: Any, **kwargs: Any):
            streaming.set()
            await asyncio.sleep(30)
            yield LLMResponse(content="never")  # pragma: no cover

        # accumulator 协议：caller 持有容器，取消后 snapshot 半截块
        class _Acc:
            state = object()

        monkeypatch.setattr(agent.model_provider, "generate", _blocking_generate)
        monkeypatch.setattr(agent.model_provider, "create_accumulator", lambda: _Acc())
        monkeypatch.setattr(agent.model_provider, "snapshot_blocks", lambda acc: blocks)

        _start_turn(agent)
        await _wait_until(streaming.is_set)
        await runtime.interrupt_session(session.session_id)

        chain = agent.context_manager.get_context_window()
        # [user, assistant(partial)] —— 半截 tool call 被剔除
        assert [m.role for m in chain] == ["user", "assistant"]
        partial = chain[1]
        assert partial.reasoning_content == "half-done reasoning"
        assert partial.content == "partial answer"
        assert partial.tool_calls is None
        assert partial.stop_reason == "interrupted"
        # 下轮请求结构合法（无悬空 tool_calls）
        _assert_well_formed(chain)

        # 未提交投影失效：补提交后当前 accumulator 置空（内容已进 messages）
        assert agent._loop.current_acc is None

        # InterruptedEvent 落盘在 partial Message 之后（链序）
        from wing.event import InterruptedEvent

        events = session.context_manager.get_active_events()
        assert any(isinstance(e, InterruptedEvent) for e in events)

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupt_with_finalized_tool_commits_synthesized_result(
        self, runtime, monkeypatch
    ):
        """打断时 tool 已终结（参数流完整）但未执行 → 合成打断结果补配对。

        回归：快照含已终结 ToolUseBlock 时原样落链会悬空——下一轮请求
        （Anthropic 要求 tool_use 后紧跟 tool_result / OpenAI 要求
        tool_calls 后有 tool 消息）结构非法 400。修复：每个已终结未执行
        调用合成 INTERRUPTED_RESULT 的 tool 消息 + 关卡片事件（与工具
        执行期打断的合成语义一致）。
        """
        session = runtime.create_session()
        agent = session.agent

        streaming = asyncio.Event()
        blocks = [
            ThinkingBlock(thinking="about to call a tool"),
            ToolUseBlock(id="call-done", name="Bash", input={"command": "ls"}),
        ]

        async def _blocking_generate(*args: Any, **kwargs: Any):
            streaming.set()
            await asyncio.sleep(30)
            yield LLMResponse(content="never")  # pragma: no cover

        class _Acc:
            state = object()

        monkeypatch.setattr(agent.model_provider, "generate", _blocking_generate)
        monkeypatch.setattr(agent.model_provider, "create_accumulator", lambda: _Acc())
        monkeypatch.setattr(agent.model_provider, "snapshot_blocks", lambda acc: blocks)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        _start_turn(agent)
        await _wait_until(streaming.is_set)
        await agent.interrupt()

        # ── 上下文：[user, assistant(thinking+tool_use), tool(合成)] ──
        chain = agent.context_manager.get_context_window()
        assert [m.role for m in chain] == ["user", "assistant", "tool"]
        assistant = chain[1]
        assert assistant.stop_reason == "interrupted"
        assert assistant.reasoning_content == "about to call a tool"
        assert [tc.id for tc in assistant.tool_calls or []] == ["call-done"]
        tool_msg = chain[2]
        assert tool_msg.tool_call_id == "call-done"
        assert tool_msg.content == _INTERRUPTED_RESULT
        _assert_well_formed(chain)

        # ── 事件：合成的关卡片事件（前端冻结流式工具卡）──
        result_events = [
            e
            for e in events
            if isinstance(e, ToolCallResultEvent) and e.tool_call_id == "call-done"
        ]
        assert len(result_events) == 1
        assert result_events[0].tool_success is False
        assert result_events[0].tool_result == _INTERRUPTED_RESULT

        # ── 打断后继续对话：下一次 LLM 调用收到合法历史（无悬空）──
        seen_messages: list[list[Message]] = []

        async def _capture_generate(*args: Any, **kwargs: Any):
            seen_messages.append(kwargs["messages"])
            yield _final_resp(content="recovered")

        monkeypatch.setattr(agent.model_provider, "generate", _capture_generate)
        await agent.post("continue")
        await _wait_until(lambda: len(seen_messages) == 1)

        _assert_well_formed(seen_messages[0])
        assert any(
            m.role == "tool" and m.content == _INTERRUPTED_RESULT
            for m in seen_messages[0]
        )

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupt_with_no_partial_content_commits_nothing(
        self, runtime, monkeypatch
    ):
        """首个 chunk 到达前打断（无已生成内容）：不产生 partial 提交。"""
        session = runtime.create_session()
        agent = session.agent

        streaming = asyncio.Event()

        async def _blocking_generate(*args: Any, **kwargs: Any):
            streaming.set()
            await asyncio.sleep(30)
            yield LLMResponse(content="never")  # pragma: no cover

        monkeypatch.setattr(agent.model_provider, "generate", _blocking_generate)
        # snapshot 返回 None（无内容可提交——真实 provider 流未开始时的行为）
        monkeypatch.setattr(agent.model_provider, "snapshot_blocks", lambda acc: None)
        monkeypatch.setattr(agent.model_provider, "create_accumulator", lambda: None)

        _start_turn(agent)
        await _wait_until(streaming.is_set)
        await agent.interrupt()

        chain = agent.context_manager.get_context_window()
        assert [m.role for m in chain] == ["user"]

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_partial_commit_persisted_across_resume(self, runtime, monkeypatch):
        """流式中断的 partial Message 经 MessageLog 落盘：resume 后可见。"""
        session = runtime.create_session()
        sid = session.session_id
        agent = session.agent

        streaming = asyncio.Event()
        blocks = [ThinkingBlock(thinking="interrupted thought")]

        async def _blocking_generate(*args: Any, **kwargs: Any):
            streaming.set()
            await asyncio.sleep(30)
            yield LLMResponse(content="never")  # pragma: no cover

        monkeypatch.setattr(agent.model_provider, "generate", _blocking_generate)
        monkeypatch.setattr(
            agent.model_provider, "create_accumulator", lambda: object()
        )
        monkeypatch.setattr(agent.model_provider, "snapshot_blocks", lambda acc: blocks)

        _start_turn(agent)
        await _wait_until(streaming.is_set)
        await agent.interrupt()
        await agent.shutdown()

        from wing.runtime import WingRuntime

        restarted = WingRuntime()
        resumed = restarted.resume_session(sid)
        chain = resumed.context_manager.get_context_window()
        assert [m.role for m in chain] == ["user", "assistant"]
        assert chain[1].reasoning_content == "interrupted thought"
        assert chain[1].stop_reason == "interrupted"

        await resumed.agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupt_idle_is_noop(self, runtime):
        session = runtime.create_session()
        agent = session.agent

        await agent.interrupt()

        assert agent.context_manager.get_context_window() == []

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_no_double_commit_after_turn_complete(self, runtime, monkeypatch):
        """turn 正常结束后再打断：不产生额外消息。"""
        session = runtime.create_session()
        agent = session.agent

        async def fast_tool(input: str = "") -> str:
            return "fast-result"

        agent._executor._tools = {"Fast": _make_tool("Fast", fast_tool)}

        done = asyncio.Event()
        event_bus.subscribe(lambda e: done.set() if isinstance(e, DoneEvent) else None)

        responses = [
            _final_resp(tool_calls=[_tc("call-1", "Fast")]),
            _final_resp(content="done"),
        ]

        async def _generate(*args: Any, **kwargs: Any):
            yield responses.pop(0)

        monkeypatch.setattr(agent.model_provider, "generate", _generate)
        _start_turn(agent)
        await asyncio.wait_for(done.wait(), timeout=5.0)

        before = list(agent.context_manager.get_context_window())
        # [user, assistant(tool_calls), tool, assistant("done")]
        assert [m.role for m in before] == ["user", "assistant", "tool", "assistant"]

        await agent.interrupt()

        after = agent.context_manager.get_context_window()
        assert len(after) == len(before)

        await agent.shutdown()


class TestPostInterruptRecovery:
    """打断后的下一轮 LLM 调用结构合法，且补提交已持久化。"""

    @pytest.mark.asyncio
    async def test_next_llm_call_is_well_formed(self, runtime, monkeypatch):
        session = runtime.create_session()
        agent = session.agent

        slow_started = asyncio.Event()

        async def fast_tool(input: str = "") -> str:
            return "fast-result"

        async def slow_tool(input: str = "") -> str:
            slow_started.set()
            await asyncio.sleep(30)
            return "never"

        agent._executor._tools = {
            "Fast": _make_tool("Fast", fast_tool),
            "Slow": _make_tool("Slow", slow_tool),
        }

        events: list[Any] = []
        event_bus.subscribe(events.append)

        seen_messages: list[list[Message]] = []

        async def _generate(*args: Any, **kwargs: Any):
            seen_messages.append(kwargs["messages"])
            if len(seen_messages) == 1:
                yield _final_resp(
                    tool_calls=[_tc("call-fast", "Fast"), _tc("call-slow", "Slow")]
                )
            else:
                yield _final_resp(content="recovered")

        monkeypatch.setattr(agent.model_provider, "generate", _generate)
        _start_turn(agent)
        await _wait_until(
            lambda: slow_started.is_set() and _result_emitted(events, "call-fast")
        )
        await agent.interrupt()
        await _wait_until(lambda: _result_emitted(events, "call-slow"))

        # 打断后继续对话：第二次 LLM 调用必须收到合法历史。
        await agent.post("continue")
        await _wait_until(lambda: len(seen_messages) == 2)

        _assert_well_formed(seen_messages[1])
        # 合成结果对模型可见
        assert any(
            m.role == "tool" and m.content == _INTERRUPTED_RESULT
            for m in seen_messages[1]
        )

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_repair_persisted_across_resume(self, runtime, monkeypatch):
        """补提交经 MessageLog 落盘：新 runtime resume 后看得到。"""
        session = runtime.create_session()
        sid = session.session_id
        agent = session.agent

        slow_started = asyncio.Event()

        async def fast_tool(input: str = "") -> str:
            return "fast-result"

        async def slow_tool(input: str = "") -> str:
            slow_started.set()
            await asyncio.sleep(30)
            return "never"

        agent._executor._tools = {
            "Fast": _make_tool("Fast", fast_tool),
            "Slow": _make_tool("Slow", slow_tool),
        }

        events: list[Any] = []
        event_bus.subscribe(events.append)

        async def _generate(*args: Any, **kwargs: Any):
            yield _final_resp(
                tool_calls=[_tc("call-fast", "Fast"), _tc("call-slow", "Slow")]
            )

        monkeypatch.setattr(agent.model_provider, "generate", _generate)
        _start_turn(agent)
        await _wait_until(
            lambda: slow_started.is_set() and _result_emitted(events, "call-fast")
        )
        await agent.interrupt()
        await _wait_until(lambda: _result_emitted(events, "call-slow"))

        await agent.shutdown()

        # 新 runtime 从磁盘恢复——修复消息必须在活跃链上。
        from wing.runtime import WingRuntime

        restarted = WingRuntime()
        resumed = restarted.resume_session(sid)
        chain = resumed.context_manager.get_context_window()
        tools = {m.tool_call_id: m.content for m in chain if m.role == "tool"}
        assert tools == {
            "call-fast": "fast-result",
            "call-slow": _INTERRUPTED_RESULT,
        }
        _assert_well_formed(chain)

        await resumed.agent.shutdown()
