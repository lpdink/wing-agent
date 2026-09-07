"""中途订阅者的完整视图测试（SyncSessionEvent 四组数据）。

任意时刻订阅 session 的客户端获得一致的完整视图，SyncSessionEvent 携带四组
重放素材，订阅方按 `messages → uncommitted → uncommitted_tools → events`
组装，随后无缝衔接 live 流：

  messages          已提交 Message 投影（活跃链）
  uncommitted       单个未提交 assistant Message 投影（已终结块，可为 null）
  uncommitted_tools 未终结 tool 调用的原始 args 片段
  events            活跃链上的**事实类**事件（FACT_EVENTS + pending ask 过滤）
  turn_started_at   当前 turn 开始时刻（UTC ISO），供前端恢复已耗时

关键结构前提（P1-1 修复）：产生 diff 的 tool_use 块是**已终结**块，因此出现在
uncommitted 投影里——diff 事件（events）重放时其锚点 ToolCall cell 已由
uncommitted 建出，锚定结构性成立。
"""

from __future__ import annotations

from typing import Any

import pytest

from wing.event import (
    AskEvent,
    DiffContentEvent,
    InterruptedEvent,
    SyncSessionEvent,
    ToolCallResultEvent,
)
from wing.event_bus import event_bus
from wing.provider.base import StreamAccumulator
from wing.provider.openai_compat import _OAIStreamState
from wing.schema import Message, PendingCall, ToolCall


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


def _set_acc(agent, *, text=None, reasoning=None, finalized_tools=None, pending=None):
    """把 agent 的当前一轮 accumulator 置为携带指定状态的容器（模拟流进行中）。"""
    state = _OAIStreamState()
    if reasoning:
        state.reasoning_chunks.append(reasoning)
    if text:
        state.content_chunks.append(text)
    for tc in finalized_tools or []:
        state.final_tool_calls.append(tc)
    for idx, pc in (pending or {}).items():
        state.pending[idx] = pc
    acc = StreamAccumulator()
    acc.state = state
    agent._loop._current_acc = acc


class TestMidTurnSubscribe:
    @pytest.mark.asyncio
    async def test_subscribe_during_streaming_sees_uncommitted(self, runtime):
        """生成途中订阅：messages + 未提交 Message 投影（含已生成 reasoning/text）。"""
        session = runtime.create_session()
        agent = session.agent

        agent.context_manager.add_message(Message(role="user", content="question"))
        agent._set_working(True)  # 进入 working（登记 turn_started_at）
        _set_acc(agent, reasoning="thinking hard", text="answer ")

        events = _collect()
        runtime.subscribe("client-late", session.session_id)

        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        # messages：已提交 Message 投影
        assert [m["role"] for m in sync.messages] == ["user"]
        # uncommitted：单个未提交 assistant Message 投影
        assert sync.uncommitted is not None
        assert sync.uncommitted["role"] == "assistant"
        assert sync.uncommitted["reasoning_content"] == "thinking hard"
        assert sync.uncommitted["content"] == "answer "
        # 无未终结调用
        assert sync.uncommitted_tools == []
        # 链上无事实事件
        assert sync.events == []
        # turn 进行中：携带开始时刻
        assert sync.turn_started_at is not None

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_subscribe_during_arg_streaming_sees_uncommitted_tools(self, runtime):
        """参数流途中订阅：未终结调用进 uncommitted_tools（原始 args，后端不解析）。"""
        session = runtime.create_session()
        agent = session.agent

        agent.context_manager.add_message(Message(role="user", content="run it"))
        agent._set_working(True)
        _set_acc(
            agent,
            pending={
                0: PendingCall(id="tc-bash", name="Bash", args_buffer='{"command": "sl')
            },
        )

        events = _collect()
        runtime.subscribe("client-late", session.session_id)

        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        assert sync.uncommitted_tools == [
            {
                "tool_call_id": "tc-bash",
                "tool_name": "Bash",
                "args_fragment": '{"command": "sl',
            }
        ]
        # 只有半截调用（无已终结块）→ Message 投影为 null
        assert sync.uncommitted is None

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_subscribe_during_tool_execution_anchors_diff(self, runtime):
        """工具执行途中订阅：uncommitted 含**已终结 tool_use**——diff 锚点结构成立。

        这是 P1-1 的结构性前提：assistant Message 尚未提交（不在 messages），
        产生 diff 的 tool_use 块已终结（在 uncommitted 投影），diff 事件已落盘
        （在 events）。前端按 messages → uncommitted → events 组装时，diff 的
        锚点 ToolCall cell 已由 uncommitted 建出。
        """
        session = runtime.create_session()
        agent = session.agent
        cm = agent.context_manager

        cm.add_message(Message(role="user", content="edit it"))
        agent._set_working(True)
        # assistant 尚未提交，但 Edit 的 tool_use 块已终结（模型已生成完参数）
        _set_acc(
            agent,
            finalized_tools=[ToolCall(id="tc-1", name="Edit", arguments={"path": "f"})],
        )
        # 工具执行期间发射的即时事实事件（落盘进链）
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
        # uncommitted：含已终结的 Edit tool_use（diff 的锚点）
        assert sync.uncommitted is not None
        assert sync.uncommitted["tool_calls"] == [
            {"id": "tc-1", "name": "Edit", "arguments": {"path": "f"}}
        ]
        # events：已落盘的 diff 事实事件，其 tool_call_id 锚定到 uncommitted 的 tool_use
        assert [e["type"] for e in sync.events] == ["diff_content"]
        assert sync.events[0]["tool_call_id"] == "tc-1"
        # 锚点（uncommitted 的 tc-1）与 diff（events 的 tc-1）一致
        anchor_ids = {tc["id"] for tc in sync.uncommitted["tool_calls"]}
        assert sync.events[0]["tool_call_id"] in anchor_ids

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_subscribe_idle_session_empty_uncommitted(self, runtime):
        """空闲 session 订阅：uncommitted null、uncommitted_tools 空、turn_started_at null。"""
        session = runtime.create_session()
        session.agent.context_manager.add_message(
            Message(role="user", content="done earlier")
        )

        events = _collect()
        runtime.subscribe("client-late", session.session_id)

        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        assert sync.uncommitted is None
        assert sync.uncommitted_tools == []
        assert sync.turn_started_at is None
        assert sync.events == []

        await session.agent.shutdown()


class TestPersistedEventsOnChain:
    @pytest.mark.asyncio
    async def test_tool_result_event_not_persisted(self, runtime):
        """tool_call_result 停止落盘（tool Message 的孪生）——也不在下发事件集。"""
        session = runtime.create_session()
        agent = session.agent

        agent.sink.tool_finished(
            ToolCall(id="tc-9", name="Bash", arguments={}),
            result="ok",
            success=True,
            model="m",
        )

        # 不落盘：活跃链上没有该事件（get_active_events 只返回事实事件）
        chain_events = agent.context_manager.get_active_events()
        assert not any(isinstance(e, ToolCallResultEvent) for e in chain_events)
        # 事件本身仍广播（走 event_bus，metrics_registry / TUI 直播依赖）
        assert agent.context_manager.get_active_events(pending_ask_ids=set()) == []

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupted_event_persisted_and_dispatched(self, runtime):
        """interrupted 是事实事件：落盘进链且在下发集合（FACT_EVENTS）。"""
        session = runtime.create_session()
        agent = session.agent
        runtime._emit_session_event(
            InterruptedEvent(session_id=session.session_id), session=session
        )
        chain_events = agent.context_manager.get_active_events()
        assert any(isinstance(e, InterruptedEvent) for e in chain_events)
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupted_event_persisted_request_id(self, runtime):
        """runtime 出口的 persist=true 事件同样在落盘前定型 request_id。"""
        import json
        import os
        from pathlib import Path

        from wing.request_context import reset_request_context, set_request_context

        session = runtime.create_session()
        sid = session.session_id

        token = set_request_context(request_id="req-int", session_id=sid)
        try:
            runtime._emit_session_event(
                InterruptedEvent(session_id=sid), session=session
            )
        finally:
            reset_request_context(token)

        history = Path(os.environ["WING_SESSIONS_PATH"], sid, "history.jsonl")
        records = [
            json.loads(line)
            for line in history.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        interrupted = [r for r in records if r.get("type") == "interrupted"]
        assert len(interrupted) == 1
        assert interrupted[0]["request_id"] == "req-int"

        await session.agent.shutdown()


class TestPendingAskFiltering:
    """6.9：pending ask 过滤——只下发仍挂起的提问（源自 inbox feedback waiters）。"""

    @pytest.mark.asyncio
    async def test_pending_ask_dispatched(self, runtime):
        """ask 挂起（waiter 存活）时下发。"""
        session = runtime.create_session()
        agent = session.agent

        agent.emit(
            AskEvent(tool_call_id="ask-1", question="proceed?", choices=["y", "n"])
        )
        agent._inbox.register_waiter("ask-1")

        events = _collect()
        runtime.subscribe("client-late", session.session_id)
        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        assert [e["type"] for e in sync.events] == ["ask"]
        assert sync.events[0]["tool_call_id"] == "ask-1"

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_answered_ask_not_dispatched(self, runtime):
        """ask 已答（waiter 注销）后不下发——重放会渲染活的 Ask 卡，回答进虚空。"""
        session = runtime.create_session()
        agent = session.agent

        agent.emit(AskEvent(tool_call_id="ask-1", question="proceed?"))
        agent._inbox.register_waiter("ask-1")
        agent._inbox.unregister_waiter("ask-1")  # 已答

        events = _collect()
        runtime.subscribe("client-late", session.session_id)
        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        assert sync.events == []

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_concurrent_asks_independent(self, runtime):
        """并发 ask：一个已答、一个仍挂起——只下发挂起的那个，tool_call_id 正确。"""
        session = runtime.create_session()
        agent = session.agent

        agent.emit(AskEvent(tool_call_id="ask-a", question="A?"))
        agent.emit(AskEvent(tool_call_id="ask-b", question="B?"))
        agent._inbox.register_waiter("ask-a")
        agent._inbox.register_waiter("ask-b")
        agent._inbox.unregister_waiter("ask-a")  # A 已答

        events = _collect()
        runtime.subscribe("client-late", session.session_id)
        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        assert [e["tool_call_id"] for e in sync.events] == ["ask-b"]

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupted_ask_not_dispatched(self, runtime):
        """中断后 waiter 被取消（cancel_all_waiters）→ ask 不下发。"""
        session = runtime.create_session()
        agent = session.agent

        agent.emit(AskEvent(tool_call_id="ask-1", question="proceed?"))
        agent._inbox.register_waiter("ask-1")
        agent._inbox.cancel_all_waiters()  # 中断

        events = _collect()
        runtime.subscribe("client-late", session.session_id)
        sync = next(e for e in events if isinstance(e, SyncSessionEvent))
        assert sync.events == []

        await agent.shutdown()
