"""UserMessageAcceptedEvent 发射语义测试。

产品语义：用户消息"开始往上渲染"当且仅当它真的被发给了模型。消费发生在
两个时刻，UserMessageAcceptedEvent 随之发射：

  1. run_turn 入口的 drain-and-merge——消息成为新一轮的输入（先于
     turn_started）；
  2. 工具执行后的 steer 注入——消息早于/期间工具调用发送。

无 request_id 的内部投递不发射。
"""

from __future__ import annotations

import asyncio
from typing import Any
from unittest.mock import patch

import pytest
from _support import mock_generate
from wing.event import (
    TextEvent,
    TurnStartedEvent,
    UserMessageAcceptedEvent,
    UserMessageCommittedEvent,
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

pytestmark = pytest.mark.usefixtures("cleanup_event_bus")


@pytest.fixture
def runtime():
    """创建 WingRuntime 实例。"""
    from wing.runtime import WingRuntime

    return WingRuntime()


def _usage() -> LLMUsage:
    return LLMUsage(prompt_tokens=10, completion_tokens=5)


async def _stop_worker(agent) -> None:
    """停止自动 worker，手动控制处理时机。"""
    agent._worker.cancel()
    try:
        await agent._worker
    except (Exception, asyncio.CancelledError):
        pass


def _put(agent, content: str, request_id: str | None = None) -> None:
    from wing.agent.inbox import Inbound
    from wing.schema import Message

    agent._inbox._enqueue(
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

        with patch.object(agent.model_provider, "generate", mock_generate):
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

        with patch.object(agent.model_provider, "generate", mock_generate):
            await agent._loop.run_turn()

        def _kind(e: Any) -> str | None:
            if isinstance(e, UserMessageCommittedEvent):
                return "committed"
            if isinstance(e, UserMessageAcceptedEvent):
                return "accepted"
            if isinstance(e, TurnStartedEvent):
                return "started"
            return None

        kinds = [k for e in events if (k := _kind(e)) is not None]
        assert kinds == ["committed", "accepted", "started"]

    @pytest.mark.asyncio
    async def test_inbound_without_request_id_commits_without_accepted(
        self, runtime: Any
    ):
        """内部投递提交到会话，但不发兼容用量事件。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)

        _put(agent, "internal-post", request_id=None)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        with patch.object(agent.model_provider, "generate", mock_generate):
            await agent._loop.run_turn()

        assert not any(isinstance(e, UserMessageAcceptedEvent) for e in events)
        committed = [e for e in events if isinstance(e, UserMessageCommittedEvent)]
        assert len(committed) == 1
        assert committed[0].origin_request_ids == []
        assert committed[0].content == "internal-post"


class TestSteerAcceptance:
    """发射点②：工具执行后的 steer 注入。"""

    @pytest.mark.asyncio
    async def test_steer_emits_accepted_after_user_commit(self, runtime: Any):
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

        chain = agent.context_manager.get_context_window()
        tool_msgs = [m for m in chain if m.role == "tool"]
        assert len(tool_msgs) == 1
        assert tool_msgs[0].content == "tool-result"
        committed = [e for e in events if isinstance(e, UserMessageCommittedEvent)]
        assert [e.origin_request_ids for e in committed] == [["req-0"], ["req-steer"]]
        assert (chain[-2].role, chain[-2].content, chain[-2].uuid) == (
            "user",
            "steer me",
            committed[-1].message_uuid,
        )

    @pytest.mark.asyncio
    @pytest.mark.parametrize("prepare_error", [False, True])
    async def test_unavailable_steer_defers_to_next_turn(
        self, runtime: Any, monkeypatch: pytest.MonkeyPatch, prepare_error: bool
    ):
        """配置关闭或启动下次调用之前的准备失败时，下轮消费同一条输入。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)
        agent._loop.steer = prepare_error
        if prepare_error:
            original = agent.context_manager.append_with_pending_compact
            failed = False

            def fail_once(message, tools):
                nonlocal failed
                if message.content == "next turn" and not failed:
                    failed = True
                    raise OSError("compact preparation failed")
                return original(message, tools)

            monkeypatch.setattr(
                agent.context_manager, "append_with_pending_compact", fail_once
            )

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
            assert agent._inbox.has_pending
            assert not any(e.type == "user_messages_dropped" for e in events)
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
