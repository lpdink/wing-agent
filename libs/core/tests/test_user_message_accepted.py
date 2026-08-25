"""UserMessageAcceptedEvent 发射语义测试。

产品语义：用户消息"开始往上渲染"当且仅当它真的被发给了模型。消费发生在
两个时刻，UserMessageAcceptedEvent 随之发射：

  1. run_turn 入口的 drain-and-merge——消息成为新一轮的输入（先于
     turn_started）；
  2. 工具执行后的 steer 注入——消息早于/期间工具调用发送。

无 request_id 的内部投递（如 Explorer 回传）不发射。
"""

from __future__ import annotations

import asyncio
from typing import Any
from unittest.mock import patch

import pytest

from wing.event import (
    TextEvent,
    TurnStartedEvent,
    UserMessageAcceptedEvent,
)
from wing.event_bus import event_bus
from wing.schema import (
    LLMResponse,
    LLMUsage,
    TextBlock,
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
    """创建 WingRuntime 实例。"""
    from wing.runtime import WingRuntime

    return WingRuntime()


def _usage() -> LLMUsage:
    return LLMUsage(prompt_tokens=10, completion_tokens=5)


async def _mock_generate(*args: Any, **kwargs: Any):
    """Mock LLM generate: 返回一条纯文本响应（无 tool calls），结束 turn。"""
    yield LLMResponse(
        content="ok",
        usage=_usage(),
    )


async def _stop_worker(agent) -> None:
    """停止自动 worker，手动控制处理时机。"""
    agent._worker.cancel()
    try:
        await agent._worker
    except (Exception, asyncio.CancelledError):
        pass


def _put(agent, content: str, request_id: str | None = None) -> None:
    from wing.agent import Inbound
    from wing.schema import Message

    agent._inbox._queue.put_nowait(
        Inbound(message=Message(role="user", content=content), request_id=request_id)
    )


class TestTurnStartAcceptance:
    """发射点①：run_turn 入口的 drain-and-merge。"""

    @pytest.mark.asyncio
    async def test_batch_emits_one_event_per_message(self, runtime: Any):
        """batch 中每条用户消息各发射一个 accepted 事件（id/内容对应）。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)

        _put(agent, "msg-A", "req-1")
        _put(agent, "msg-B", "req-2")
        _put(agent, "msg-C", "req-3")

        events: list[Any] = []
        event_bus.subscribe(events.append)

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await agent._loop.run_turn()

        accepted = [e for e in events if isinstance(e, UserMessageAcceptedEvent)]
        assert [(e.origin_request_id, e.content) for e in accepted] == [
            ("req-1", "msg-A"),
            ("req-2", "msg-B"),
            ("req-3", "msg-C"),
        ]

    @pytest.mark.asyncio
    async def test_accepted_precedes_turn_started(self, runtime: Any):
        """事件流顺序：accepted 全部先于 turn_started（渲染顺序即提交顺序）。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)

        _put(agent, "hello", "req-1")

        events: list[Any] = []
        event_bus.subscribe(events.append)

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await agent._loop.run_turn()

        def _kind(e: Any) -> str | None:
            if isinstance(e, UserMessageAcceptedEvent):
                return "accepted"
            if isinstance(e, TurnStartedEvent):
                return "started"
            return None

        kinds = [k for e in events if (k := _kind(e)) is not None]
        assert kinds == ["accepted", "started"]

    @pytest.mark.asyncio
    async def test_no_event_for_inbound_without_request_id(self, runtime: Any):
        """内部投递（无 request_id，如 Explorer 回传）不发射。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)

        _put(agent, "internal-post", request_id=None)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await agent._loop.run_turn()

        assert not any(isinstance(e, UserMessageAcceptedEvent) for e in events)


class TestSteerAcceptance:
    """发射点②：工具执行后的 steer 注入。"""

    @pytest.mark.asyncio
    async def test_steer_emits_accepted_and_injects_note(self, runtime: Any):
        """工具执行期间排队的消息：steer drain 时发射 accepted，
        且事件落在工具结果事件之后、下一轮 text 事件之前。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)

        async def _echo(input: str = "") -> str:
            # 模拟用户在工具执行期间发消息
            _put(agent, "steer me", "req-steer")
            return "tool-result"

        agent._executor._tools = {
            "Echo": Tool(
                name="Echo",
                description="echo tool",
                params=[ToolParam(name="input", type="string", description="input")],
                function=_echo,
            ),
        }

        tc = ToolCall(id="call-1", name="Echo", arguments={"input": "x"})
        calls = iter(
            [
                LLMResponse(
                    tool_calls=[tc],
                    content_blocks=[
                        ToolUseBlock(id=tc.id, name=tc.name, input=tc.arguments)
                    ],
                    usage=_usage(),
                ),
                LLMResponse(
                    content="done",
                    content_blocks=[TextBlock(text="done")],
                    usage=_usage(),
                ),
            ]
        )

        async def _generate(*args: Any, **kwargs: Any):
            yield next(calls)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        _put(agent, "go", "req-0")
        with patch.object(agent.model_provider, "generate", _generate):
            await agent._loop.run_turn()

        accepted = [e for e in events if isinstance(e, UserMessageAcceptedEvent)]
        assert [(x.origin_request_id, x.content) for x in accepted] == [
            ("req-0", "go"),
            ("req-steer", "steer me"),
        ]

        # 顺序：steer accepted 晚于工具结果事件、早于下一轮 text 事件
        from wing.event import ToolCallResultEvent

        idx_tool_result = next(
            i
            for i, e in enumerate(events)
            if isinstance(e, ToolCallResultEvent) and e.tool_call_id == "call-1"
        )
        idx_steer_accepted = next(
            i
            for i, e in enumerate(events)
            if isinstance(e, UserMessageAcceptedEvent)
            and e.origin_request_id == "req-steer"
        )
        idx_text = next(i for i, e in enumerate(events) if isinstance(e, TextEvent))
        assert idx_tool_result < idx_steer_accepted < idx_text

        # steer note 注入了最后一个工具结果
        chain = agent.context_manager.get_context_window()
        tool_msgs = [m for m in chain if m.role == "tool"]
        assert len(tool_msgs) == 1
        assert tool_msgs[0].content == "[User steer note: steer me]\ntool-result"

    @pytest.mark.asyncio
    async def test_steer_disabled_defers_to_next_turn(self, runtime: Any):
        """steer 关闭时消息留在 inbox，由下一轮 run_turn 发射。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)
        agent._loop.steer = False

        async def _echo(input: str = "") -> str:
            _put(agent, "next turn", "req-late")
            return "tool-result"

        agent._executor._tools = {
            "Echo": Tool(
                name="Echo",
                description="echo tool",
                params=[ToolParam(name="input", type="string", description="input")],
                function=_echo,
            ),
        }

        tc = ToolCall(id="call-1", name="Echo", arguments={"input": "x"})
        calls = iter(
            [
                LLMResponse(
                    tool_calls=[tc],
                    content_blocks=[
                        ToolUseBlock(id=tc.id, name=tc.name, input=tc.arguments)
                    ],
                    usage=_usage(),
                ),
                LLMResponse(
                    content="done",
                    content_blocks=[TextBlock(text="done")],
                    usage=_usage(),
                ),
                LLMResponse(
                    content="ok",
                    content_blocks=[TextBlock(text="ok")],
                    usage=_usage(),
                ),
            ]
        )

        async def _generate(*args: Any, **kwargs: Any):
            yield next(calls)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        _put(agent, "go", "req-0")
        with patch.object(agent.model_provider, "generate", _generate):
            await agent._loop.run_turn()
            # 第一条消息的 turn 结束后，"next turn" 仍在 inbox
            assert not any(
                isinstance(e, UserMessageAcceptedEvent)
                and e.origin_request_id == "req-late"
                for e in events
            )
            await agent._loop.run_turn()

        accepted = [
            (e.origin_request_id, e.content)
            for e in events
            if isinstance(e, UserMessageAcceptedEvent)
        ]
        assert ("req-late", "next turn") in accepted
