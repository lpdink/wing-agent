"""中途订阅者的完整视图测试（SyncSessionEvent.events / in_flight）。

任意时刻订阅 session 的客户端获得一致的完整视图：
  已落盘事件（活跃链事件节点，按链序）
  + in-flight 瞬态事件（journal 合成包，按发生序）
  + 后续 live 流
组装顺序由订阅方保证为 messages+events → in_flight → live。
"""

from __future__ import annotations

from typing import Any

import pytest

from wing.event import (
    DiffContentEvent,
    SyncSessionEvent,
    ToolCallResultEvent,
)
from wing.event_bus import event_bus
from wing.schema import Message, ToolCall


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


def _collect() -> list[Any]:
    events: list[Any] = []
    event_bus.subscribe(events.append)
    return events


class TestMidTurnSubscribe:
    @pytest.mark.asyncio
    async def test_subscribe_during_streaming_sees_in_flight(self, runtime):
        """生成途中订阅：messages + 空 events + in_flight 合成包。"""
        session = runtime.create_session()
        agent = session.agent

        # 已提交 user 消息；turn 进行中：journal 缓冲流式 delta（经 sink 分流）
        agent.context_manager.add_message(Message(role="user", content="question"))
        agent.sink.llm_reasoning("thinking ")
        agent.sink.llm_reasoning("hard")
        agent.sink.llm_text("answer ")

        events = _collect()
        runtime.subscribe("client-late", session.session_id)

        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        # messages：Message 投影
        assert [m["role"] for m in sync.messages] == ["user"]
        # events：链上无事件节点
        assert sync.events == []
        # in_flight：两条 delta 合成为两个包（reasoning 同类合并 + text）
        assert [e["type"] for e in sync.in_flight] == ["reasoning", "text"]
        assert sync.in_flight[0]["content"] == "thinking hard"
        assert sync.in_flight[1]["content"] == "answer "

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_subscribe_during_tool_execution(self, runtime):
        """工具执行途中订阅：链上事件（diff）+ journal 瞬态（ToolCallEvent）。"""
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        # turn 进行中：user 已提交；assistant 尚未提交（工具执行中）
        cm.add_message(Message(role="user", content="edit it"))
        # 工具执行期间的即时事件（经 sink 分流落盘进链）
        agent.sink.tool_started(
            ToolCall(id="tc-1", name="Edit", arguments={"path": "f"})
        )
        agent.emit(
            DiffContentEvent(
                path="f", old_text=None, new_text="new content", tool_call_id="tc-1"
            )
        )

        events = _collect()
        runtime.subscribe("client-late", session.session_id)

        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        # messages：只有 user（assistant 未提交）
        assert [m["role"] for m in sync.messages] == ["user"]
        # events：链上已落盘的 diff 事件
        assert [e["type"] for e in sync.events] == ["diff_content"]
        assert sync.events[0]["tool_call_id"] == "tc-1"
        # in_flight：ToolCallEvent（瞬态，工具 cell 重建素材）
        assert [e["type"] for e in sync.in_flight] == ["tool_call"]

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_subscribe_idle_session_no_in_flight(self, runtime):
        """空闲 session 订阅：in_flight 为空（无进行中 turn）。"""
        session = runtime.create_session()
        session.agent.context_manager.add_message(
            Message(role="user", content="done earlier")
        )

        events = _collect()
        runtime.subscribe("client-late", session.session_id)

        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        assert sync.in_flight == []
        assert sync.events == []

        await session.agent.shutdown()


class TestPersistedEventsOnChain:
    @pytest.mark.asyncio
    async def test_tool_finished_persists_result_event(self, runtime):
        """tool_finished 的 ToolCallResultEvent 即时落盘进链。"""
        session = runtime.create_session()
        agent = session.agent

        agent.sink.tool_finished(
            ToolCall(id="tc-9", name="Bash", arguments={}),
            result="ok",
            success=True,
            model="m",
        )

        chain_events = agent.context_manager.get_active_events()
        assert len(chain_events) == 1
        ev = chain_events[0]
        assert isinstance(ev, ToolCallResultEvent)
        assert ev.tool_call_id == "tc-9"
        assert ev.tool_success is True

        await agent.shutdown()
