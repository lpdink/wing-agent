"""会话用量账本（SessionUsage / UsageTracker）——累计、落盘与跨会话延续。

锚点：
- 账本口径：事件驱动增量（与直播同源），回合收尾落盘，释放前 / 进程退出前
  的显式落盘点兜底；
- 续存：resume（同一 session id 重新水合）后基线来自盘上记录，继续累计；
- API：``/api/session/info`` 的投影携带全量账本。
"""

from __future__ import annotations

import pytest

from wing.event import LLMCallMetricsEvent, ToolCallResultEvent
from wing.event_bus import event_bus
from wing.gateway.projection import build_session_info
from wing.schema import LLMUsage, ToolCall
from wing.session.usage import UsageTracker
from wing.store import SessionStore, SessionUsage


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    """每个测试前后清理全局 EventBus（Session 构造即订阅）。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


def _usage_of(store: SessionStore, session_id: str) -> SessionUsage | None:
    metadata = store.load_metadata(session_id)
    return metadata.usage if metadata is not None else None


# ============================================================
# UsageTracker — 纯累计 / 节流 / 落盘
# ============================================================


class _Clock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now


def test_tracker_accumulates_every_dimension():
    persisted: list[SessionUsage] = []
    tracker = UsageTracker(persist=persisted.append)

    tracker.record_user_message()
    tracker.record_llm_call(
        prompt_tokens=100, completion_tokens=20, cached_tokens=40, duration_ms=1200
    )
    tracker.record_tool_call(duration_ms=30)
    tracker.record_turn()

    usage = tracker.snapshot()
    assert usage.user_messages == 1
    assert usage.llm_calls == 1
    assert usage.prompt_tokens == 100
    assert usage.completion_tokens == 20
    assert usage.cached_tokens == 40
    assert usage.llm_time_ms == 1200
    assert usage.tool_calls == 1
    assert usage.tool_time_ms == 30
    assert usage.turns == 1
    assert persisted == []


def test_tracker_seeds_from_the_recorded_baseline():
    tracker = UsageTracker(
        SessionUsage(prompt_tokens=7, llm_calls=1),
        persist=lambda usage: None,
    )
    tracker.record_llm_call(
        prompt_tokens=3, completion_tokens=1, cached_tokens=0, duration_ms=5
    )
    usage = tracker.snapshot()
    assert usage.prompt_tokens == 10
    assert usage.llm_calls == 2
    assert usage.completion_tokens == 1


def test_tracker_skips_synthesized_tool_results():
    """中断合成的结果（duration None）不是一次执行：不计数、不计时。"""
    tracker = UsageTracker(persist=lambda usage: None)
    tracker.record_tool_call(duration_ms=None)
    assert tracker.snapshot().tool_calls == 0
    assert tracker.snapshot().tool_time_ms == 0


def test_tracker_throttles_flushes_and_flush_is_idempotent():
    clock = _Clock()
    persisted: list[SessionUsage] = []
    tracker = UsageTracker(persist=persisted.append, flush_interval_s=10.0, clock=clock)

    tracker.record_user_message()  # 距上次落盘 0s：只标脏
    assert persisted == []
    tracker.flush()
    assert len(persisted) == 1

    tracker.record_user_message()
    tracker.flush()  # 显式落盘点：有脏就写，不等节流
    assert len(persisted) == 2
    assert persisted[-1].user_messages == 2

    tracker.flush()  # 无未落盘增量：no-op
    assert len(persisted) == 2

    clock.now = 10.0
    tracker.record_user_message()  # 节流到期：记完即落
    assert len(persisted) == 3
    assert persisted[-1].user_messages == 3


def test_tracker_keeps_dirty_on_persist_failure():
    clock = _Clock()
    calls: list[SessionUsage] = []

    def persist(usage: SessionUsage) -> None:
        calls.append(usage)
        if len(calls) == 1:
            raise OSError("disk full")

    tracker = UsageTracker(persist=persist, flush_interval_s=10.0, clock=clock)
    tracker.record_user_message()
    tracker.flush()  # 第一次失败：脏标记保留
    tracker.flush()  # 显式落盘点重试
    assert len(calls) == 2
    assert calls[-1].user_messages == 1


# ============================================================
# Session 接线 — 事件驱动记账 + 落盘 + 续存
# ============================================================


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


@pytest.mark.asyncio
async def test_session_records_usage_from_live_events(runtime):
    session = runtime.create_session()
    sink = session.agent.sink
    tc = ToolCall(id="c1", name="Bash", arguments={})

    assert session.usage == SessionUsage()
    # 未产生任何用量前不落记录（无写噪声）。
    assert _usage_of(session.store, session.session_id) is None

    sink.user_message_accepted("hello", "r1")
    sink.llm_metrics(
        LLMUsage(prompt_tokens=100, completion_tokens=20, cached_tokens=40),
        duration_ms=1200,
    )
    sink.tool_finished(tc, "ok", success=True, model="m", duration_ms=30)
    sink.tool_finished(tc, "interrupted", success=False, model="m")  # 合成结果

    usage = session.usage
    assert usage.user_messages == 1
    assert usage.llm_calls == 1
    assert usage.prompt_tokens == 100
    assert usage.completion_tokens == 20
    assert usage.cached_tokens == 40
    assert usage.llm_time_ms == 1200
    assert usage.tool_calls == 1
    assert usage.tool_time_ms == 30
    assert usage.turns == 0
    # 回合未收尾：不落盘（节流窗口内），盘上仍是旧值。
    assert _usage_of(session.store, session.session_id) is None

    sink.turn_result(subtype="success", num_turns=1, duration_ms=1500)

    assert session.usage.turns == 1
    # 回合收尾 = 落盘点：读数与盘上一致（API 与 metadata 同源）。
    recorded = _usage_of(session.store, session.session_id)
    assert recorded is not None
    assert recorded == session.usage


@pytest.mark.asyncio
async def test_usage_survives_resume_and_keeps_counting(runtime):
    session = runtime.create_session()
    sid = session.session_id
    session.agent.sink.llm_metrics(
        LLMUsage(prompt_tokens=10, completion_tokens=4, cached_tokens=2),
        duration_ms=50,
    )
    session.agent.sink.turn_result(subtype="success", num_turns=1, duration_ms=60)

    assert runtime.sm.evict(sid, reason="test")
    await runtime.sm.wait_teardowns()

    resumed = runtime.sm.resume_session(sid)
    assert resumed.usage.prompt_tokens == 10
    assert resumed.usage.llm_calls == 1

    resumed.agent.sink.llm_metrics(
        LLMUsage(prompt_tokens=5, completion_tokens=3, cached_tokens=1),
        duration_ms=25,
    )
    resumed.agent.sink.turn_result(subtype="success", num_turns=1, duration_ms=30)

    assert resumed.usage.prompt_tokens == 15
    assert resumed.usage.completion_tokens == 7
    assert resumed.usage.cached_tokens == 3
    assert resumed.usage.llm_time_ms == 75
    assert resumed.usage.llm_calls == 2
    assert resumed.usage.turns == 2


@pytest.mark.asyncio
async def test_aclose_flushes_mid_turn_accumulation(runtime):
    session = runtime.create_session()
    session.agent.sink.llm_metrics(
        LLMUsage(prompt_tokens=8, completion_tokens=2, cached_tokens=0),
        duration_ms=11,
    )
    assert _usage_of(session.store, session.session_id) is None

    await session.aclose()  # 释放前落盘

    recorded = _usage_of(session.store, session.session_id)
    assert recorded is not None and recorded.prompt_tokens == 8


@pytest.mark.asyncio
async def test_manager_flush_usage_covers_a_running_turn(runtime):
    """进程退出前的落盘点：回合未收尾的累计也不丢。"""
    session = runtime.create_session()
    session.agent.sink.user_message_accepted("hi", "r1")

    runtime.sm.flush_usage()

    recorded = _usage_of(session.store, session.session_id)
    assert recorded is not None and recorded.user_messages == 1


@pytest.mark.asyncio
async def test_events_of_other_sessions_are_ignored(runtime):
    first = runtime.create_session()
    second = runtime.create_session()

    event_bus.emit(
        LLMCallMetricsEvent(
            session_id=first.session_id,
            prompt_tokens=9,
            completion_tokens=1,
            cached_tokens=0,
            first_chunk_rt_ms=1.0,
            tokens_per_sec=1.0,
            duration_ms=10,
        )
    )
    assert first.usage.prompt_tokens == 9
    assert second.usage == SessionUsage()


@pytest.mark.asyncio
async def test_session_info_projection_carries_usage(runtime):
    session = runtime.create_session()
    session.agent.sink.llm_metrics(
        LLMUsage(prompt_tokens=20, completion_tokens=5, cached_tokens=10),
        duration_ms=100,
    )
    session.agent.sink.tool_finished(
        ToolCall(id="c1", name="Bash", arguments={}),
        "ok",
        success=True,
        model="m",
        duration_ms=7,
    )

    usage = build_session_info(session).usage
    assert usage.prompt_tokens == 20
    assert usage.completion_tokens == 5
    assert usage.cached_tokens == 10
    assert usage.llm_time_ms == 100
    assert usage.tool_calls == 1
    assert usage.tool_time_ms == 7


@pytest.mark.asyncio
async def test_persist_failure_keeps_the_live_ledger(
    runtime, monkeypatch, wing_warnings
):
    """盘写失败只降级：live 账本照常读数，warning 留痕，turn 不被打断。"""
    session = runtime.create_session()

    def _boom(*args, **kwargs):
        raise OSError("disk full")

    monkeypatch.setattr(session.store, "save_metadata", _boom)

    session.agent.sink.user_message_accepted("hi", "r1")
    session.agent.sink.turn_result(subtype="success", num_turns=1, duration_ms=1)

    assert session.usage.user_messages == 1
    assert session.usage.turns == 1
    assert any("usage not persisted" in message for message in wing_warnings), (
        wing_warnings
    )


@pytest.mark.asyncio
async def test_closed_session_ignores_late_events(runtime):
    """aclose 后事件不再记账（退订）——逐出后的残留事件不复活会话。"""
    session = runtime.create_session()
    await session.aclose()

    event_bus.emit(
        ToolCallResultEvent(
            session_id=session.session_id,
            tool_name="Bash",
            tool_args={},
            tool_call_id="c1",
            tool_result="late",
            tool_success=True,
            duration_ms=5,
        )
    )
    assert session.usage.tool_calls == 0
