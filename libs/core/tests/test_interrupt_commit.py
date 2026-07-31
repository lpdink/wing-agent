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

流式三段（reasoning / content / tool 参数流式）中被打断时 LLM 响应未完成，
按原语义丢弃，上下文不变。
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from wing.agent import Inbound, WingAgent, _INTERRUPTED_RESULT
from wing.event import (
    AskEvent,
    DoneEvent,
    ToolCallResultEvent,
    ToolResultTurnEvent,
)
from wing.event_bus import event_bus
from wing.schema import LLMResponse, LLMUsage, Message, Tool, ToolCall, ToolParam


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


def _usage() -> LLMUsage:
    return LLMUsage(prompt_tokens=10, completion_tokens=5)


async def _wait_until(pred, timeout: float = 5.0) -> None:
    """轮询等待 pred() 为真。"""

    async def _inner():
        while not pred():
            await asyncio.sleep(0.005)

    await asyncio.wait_for(_inner(), timeout)


def _start_turn(agent: WingAgent, content: str = "go") -> None:
    agent._inbox.put_nowait(Inbound(message=Message(role="user", content=content)))


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

        agent._tools = {
            "Fast": _make_tool("Fast", fast_tool),
            "Slow": _make_tool("Slow", slow_tool),
        }

        events: list[Any] = []
        event_bus.subscribe(events.append)

        calls = [_tc("call-fast", "Fast"), _tc("call-slow", "Slow")]

        async def _generate(*args: Any, **kwargs: Any):
            yield LLMResponse(tool_calls=calls, usage=_usage())

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

        agent._tools = {"Ask": _make_tool("Ask", asking_tool)}

        events: list[Any] = []
        event_bus.subscribe(events.append)

        async def _generate(*args: Any, **kwargs: Any):
            yield LLMResponse(tool_calls=[_tc("call-ask", "Ask")], usage=_usage())

        monkeypatch.setattr(agent.model_provider, "generate", _generate)
        _start_turn(agent)
        await _wait_until(lambda: "call-ask" in agent._feedback_waiters)
        await agent.interrupt()
        await _wait_until(lambda: _result_emitted(events, "call-ask"))

        chain = agent.context_manager.get_context_window()
        tools = {m.tool_call_id: m.content for m in chain if m.role == "tool"}
        assert tools == {"call-ask": _INTERRUPTED_RESULT}
        _assert_well_formed(chain)
        assert not agent._feedback_waiters

        await agent.shutdown()


class TestInterruptOutsideToolExec:
    """工具执行之外的打断：不补提交（半截响应丢弃 / 空闲 no-op）。"""

    @pytest.mark.asyncio
    async def test_interrupt_during_streaming_discards_partial(
        self, runtime, monkeypatch
    ):
        session = runtime.create_session()
        agent = session.agent

        streaming = asyncio.Event()

        async def _blocking_generate(*args: Any, **kwargs: Any):
            streaming.set()
            await asyncio.sleep(30)
            yield LLMResponse(content="never")  # pragma: no cover

        monkeypatch.setattr(agent.model_provider, "generate", _blocking_generate)
        _start_turn(agent)
        # 事件在生成器体内设置——此刻 worker 必挂在该生成器的 await 上。
        await _wait_until(streaming.is_set)
        await agent.interrupt()

        # 只剩 turn 开始时注入的 user 消息，半截响应未入库。
        chain = agent.context_manager.get_context_window()
        assert [m.role for m in chain] == ["user"]

        await agent.shutdown()

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

        agent._tools = {"Fast": _make_tool("Fast", fast_tool)}

        done = asyncio.Event()
        event_bus.subscribe(lambda e: done.set() if isinstance(e, DoneEvent) else None)

        responses = [
            LLMResponse(tool_calls=[_tc("call-1", "Fast")], usage=_usage()),
            LLMResponse(content="done", usage=_usage()),
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

        agent._tools = {
            "Fast": _make_tool("Fast", fast_tool),
            "Slow": _make_tool("Slow", slow_tool),
        }

        events: list[Any] = []
        event_bus.subscribe(events.append)

        seen_messages: list[list[Message]] = []

        async def _generate(*args: Any, **kwargs: Any):
            seen_messages.append(kwargs["messages"])
            if len(seen_messages) == 1:
                yield LLMResponse(
                    tool_calls=[_tc("call-fast", "Fast"), _tc("call-slow", "Slow")],
                    usage=_usage(),
                )
            else:
                yield LLMResponse(content="recovered", usage=_usage())

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

        agent._tools = {
            "Fast": _make_tool("Fast", fast_tool),
            "Slow": _make_tool("Slow", slow_tool),
        }

        events: list[Any] = []
        event_bus.subscribe(events.append)

        async def _generate(*args: Any, **kwargs: Any):
            yield LLMResponse(
                tool_calls=[_tc("call-fast", "Fast"), _tc("call-slow", "Slow")],
                usage=_usage(),
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
