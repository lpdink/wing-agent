"""Tests for inbox drain-and-merge semantics.

Inbox 消费语义为 drain-and-merge：block 等待首条消息，再 non-blocking
取出剩余消息，将所有 user content 拼接为一条消息注入 context。
"""

from __future__ import annotations

import asyncio
from typing import Any
from unittest.mock import patch

import pytest

from wing.event_bus import event_bus
from wing.schema import LLMResponse, LLMUsage


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


async def _mock_generate(*args: Any, **kwargs: Any):
    """Mock LLM generate: 返回一条纯文本响应（无 tool calls），结束 turn。"""
    yield LLMResponse(
        content="ok",
        usage=LLMUsage(prompt_tokens=10, completion_tokens=5),
    )


class TestDrainAndMerge:
    """Inbox drain-and-merge 消费语义测试。"""

    @pytest.mark.asyncio
    async def test_batch_merge_multiple_queued_messages(self, runtime: Any):
        """多条排队消息应合并为单条 user message 进入 context。"""
        session = runtime.create_session()
        agent = session.agent

        # 停止自动 worker，手动控制处理时机
        agent._worker.cancel()
        try:
            await agent._worker
        except (Exception, asyncio.CancelledError):
            pass

        # 直接入队 3 条消息（模拟快速连续 post）
        from wing.agent import Inbound
        from wing.schema import Message

        agent._inbox.put_nowait(
            Inbound(message=Message(role="user", content="msg-A"), request_id="req-1")
        )
        agent._inbox.put_nowait(
            Inbound(message=Message(role="user", content="msg-B"), request_id="req-2")
        )
        agent._inbox.put_nowait(
            Inbound(message=Message(role="user", content="msg-C"), request_id="req-3")
        )

        # Mock LLM 并手动触发一次 _process_turn
        with patch.object(agent.model_provider, "generate", _mock_generate):
            await agent._process_turn()

        # 断言：context 中只有一条 user message，内容为三者拼接
        messages = agent.context_manager._messages.active_chain
        user_msgs = [m for m in messages if m.role == "user"]
        assert len(user_msgs) == 1
        assert user_msgs[0].content == "msg-A\nmsg-B\nmsg-C"

    @pytest.mark.asyncio
    async def test_batch_uses_first_request_id(self, runtime: Any):
        """Batch 合并后 request_id 取首条消息的。"""
        session = runtime.create_session()
        agent = session.agent

        agent._worker.cancel()
        try:
            await agent._worker
        except (Exception, asyncio.CancelledError):
            pass

        from wing.agent import Inbound
        from wing.schema import Message

        agent._inbox.put_nowait(
            Inbound(message=Message(role="user", content="first"), request_id="req-A")
        )
        agent._inbox.put_nowait(
            Inbound(message=Message(role="user", content="second"), request_id="req-B")
        )

        # 通过 event_bus 捕获事件，验证 request_id 取首条
        captured_request_ids: list[str | None] = []

        def _capture(event: Any) -> None:
            rid = getattr(event, "request_id", None)
            if rid is not None:
                captured_request_ids.append(rid)

        event_bus.subscribe(_capture)

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await agent._process_turn()

        # 所有事件应携带首条消息的 request_id
        assert len(captured_request_ids) > 0
        assert all(rid == "req-A" for rid in captured_request_ids)

    @pytest.mark.asyncio
    async def test_single_message_no_change(self, runtime: Any):
        """单条消息行为不变——无合并、无拼接。"""
        session = runtime.create_session()
        agent = session.agent

        agent._worker.cancel()
        try:
            await agent._worker
        except (Exception, asyncio.CancelledError):
            pass

        from wing.agent import Inbound
        from wing.schema import Message

        agent._inbox.put_nowait(
            Inbound(message=Message(role="user", content="solo"), request_id="req-1")
        )

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await agent._process_turn()

        messages = agent.context_manager._messages.active_chain
        user_msgs = [m for m in messages if m.role == "user"]
        assert len(user_msgs) == 1
        assert user_msgs[0].content == "solo"
