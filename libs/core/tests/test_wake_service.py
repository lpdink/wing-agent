"""tests/test_wake_service.py — 唤醒服务的生命周期 / 事件分发 / 投递 / 重启恢复。

覆盖（全部注入假时钟与假 ports，零 sleep、零真实会话）：
  - 创建期校验（缺省值、target==source、非法 spec / on_events / times 组合、源或目标不可解析）；
  - cancel / list 的状态语义（幂等 no-op、终态仍在 ``--all`` 里）；
  - 事件分发：done 消耗配额（归零即终结）、ask 不消耗、on_events 过滤、多条触发器各发一次；
  - 投递失败 → ``failed`` + ``last_error``，异常不外溢（唤醒失败永不影响 source）；
  - 定时触发：timer 信封的 ``#n``、预算耗尽即终结；
  - 重启恢复两分支：一次性补发一次后终结 / 周期型只重新对齐不补发；
  - DB 不可用：不武装、不抛、创建报 WakeUnavailableError。
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

from wing.db.database import Database
from wing.event import AskEvent, DoneEvent, TurnResultEvent
from wing.event_bus import event_bus
from wing.wake import WakeService, WakeUnavailableError
from wing.wake.model import (
    EVENT_DONE,
    STATE_ACTIVE,
    STATE_CANCELED,
    STATE_DONE,
    STATE_FAILED,
)

CHILD = "20260101-120000-child"
PARENT = "20260101-120000-parent"
OTHER = "20260101-120000-other"


class Clock:
    """可推进的假时钟（epoch 毫秒）。"""

    def __init__(self, now_ms: int = 1_800_000_000_000) -> None:
        self.now = now_ms

    def __call__(self) -> int:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += int(seconds * 1000)


class FakePorts:
    """``WakePorts`` 的替身：记录投递、可注入失败。"""

    def __init__(self) -> None:
        self.existing: set[str] = {CHILD, PARENT, OTHER}
        self.archived: set[str] = set()
        self.delivered: list[tuple[str, str, str]] = []
        self.error: BaseException | None = None

    def session_exists(self, session_id: str) -> bool:
        return session_id in self.existing

    def is_archived(self, session_id: str) -> bool:
        return session_id in self.archived

    async def deliver(self, *, session_id: str, content: str, request_id: str) -> None:
        if self.error is not None:
            raise self.error
        self.delivered.append((session_id, content, request_id))


@pytest.fixture(autouse=True)
def clean_event_bus():
    """EventBus 是全局单例：每个测试前后清空订阅与路由。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def clock() -> Clock:
    return Clock()


@pytest.fixture
def ports() -> FakePorts:
    return FakePorts()


@pytest.fixture
def db(tmp_path: Path) -> Database:
    database = Database(tmp_path / "wing.db")
    assert database.ready
    return database


@pytest.fixture
def service(ports: FakePorts, db: Database, clock: Clock) -> WakeService:
    wake = WakeService(ports, database=db, clock=clock)
    wake.install()
    return wake


def _done(session_id: str = CHILD, **overrides: Any) -> TurnResultEvent:
    payload: dict[str, Any] = {
        "session_id": session_id,
        "subtype": "success",
        "num_turns": 2,
        "duration_ms": 1500,
        "result": "all good",
    }
    payload.update(overrides)
    return TurnResultEvent.model_validate(payload)


def _ask(session_id: str = CHILD, **overrides: Any) -> AskEvent:
    payload: dict[str, Any] = {
        "session_id": session_id,
        "tool_call_id": "tc-1",
        "questions": [
            {
                "header": "Color",
                "question": "Which?",
                "options": [{"label": "red"}, {"label": "blue"}],
            }
        ],
    }
    payload.update(overrides)
    return AskEvent.model_validate(payload)


def _session_trigger(service: WakeService, **overrides: Any):
    payload: dict[str, Any] = {
        "kind": "session",
        "target_session_id": PARENT,
        "source_session_id": CHILD,
    }
    payload.update(overrides)
    return service.create(**payload)


def _timer_trigger(service: WakeService, **overrides: Any):
    payload: dict[str, Any] = {
        "kind": "timer",
        "target_session_id": PARENT,
        "spec": "every:30m",
        "prompt": "ping",
    }
    payload.update(overrides)
    return service.create(**payload)


async def _drain() -> None:
    """让 ``create_task`` 排下的分发任务跑完（不猜时长，只让出事件循环）。"""
    pending = [
        task
        for task in asyncio.all_tasks()
        if task is not asyncio.current_task() and not task.done()
    ]
    if pending:
        await asyncio.gather(*pending)


# ============================================================
# 创建
# ============================================================


class TestCreate:
    def test_session_defaults(self, service: WakeService) -> None:
        trigger = _session_trigger(service)
        assert trigger.kind == "session"
        assert trigger.on_events == "done,ask"
        assert trigger.spec is None
        assert trigger.next_fire_at_ms is None
        # 缺省配额 1：done 送达即整条终结
        assert trigger.remaining_done == 1
        assert trigger.state == STATE_ACTIVE
        assert trigger.fire_count == 0
        assert service.list_triggers() == [trigger]

    def test_session_times_sets_quota(self, service: WakeService) -> None:
        assert _session_trigger(service, times=3).remaining_done == 3

    def test_session_prompt_is_kept(self, service: WakeService) -> None:
        assert _session_trigger(service, prompt="hi").prompt == "hi"

    def test_session_rejects_spec(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            _session_trigger(service, spec="in:5m")

    def test_self_wake_is_rejected(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            _session_trigger(service, target_session_id=CHILD)

    def test_unknown_kind(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            service.create(kind="cron", target_session_id=PARENT)

    def test_bad_on_events(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            _session_trigger(service, on_events="done,error")
        with pytest.raises(ValueError):
            _session_trigger(service, on_events="  ")

    def test_source_is_required_for_session(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            service.create(kind="session", target_session_id=PARENT)

    def test_unknown_source(self, service: WakeService) -> None:
        with pytest.raises(LookupError):
            _session_trigger(service, source_session_id="ghost")

    def test_archived_source_is_explained(self, service: WakeService, ports) -> None:
        ports.archived.add(CHILD)
        ports.existing.discard(CHILD)
        with pytest.raises(LookupError, match="archived"):
            _session_trigger(service)

    def test_unknown_target(self, service: WakeService) -> None:
        with pytest.raises(LookupError):
            _session_trigger(service, target_session_id="ghost")

    def test_timer_defaults(self, service: WakeService, clock: Clock) -> None:
        trigger = _timer_trigger(service)
        assert trigger.kind == "timer"
        assert trigger.spec == "every:30m"
        assert trigger.next_fire_at_ms == clock.now + 1_800_000
        assert trigger.remaining_done is None  # 无限
        assert trigger.prompt == "ping"

    def test_timer_requires_prompt_and_spec(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            _timer_trigger(service, prompt="")
        with pytest.raises(ValueError):
            _timer_trigger(service, spec=None)

    def test_timer_rejects_source(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            _timer_trigger(service, source_session_id=CHILD)

    def test_one_shot_spec_rejects_times(self, service: WakeService) -> None:
        for spec in ("in:5m", "at:2026-03-01 09:00"):
            with pytest.raises(ValueError, match="times"):
                _timer_trigger(service, spec=spec, times=2)

    def test_times_must_be_positive(self, service: WakeService) -> None:
        with pytest.raises(ValueError):
            _timer_trigger(service, times=0)


class TestSameSubscriptionUpserts:
    """同一 (source, target, on_events) 再登记一次 = 刷新，不是再插一行。

    一条 active 订阅表达的是「下一次完成时告诉我」这个**状态**。多插一行会让同一个
    ask / 同一个 done 被投递多次（`ask` 从不消耗配额，所以这个放大在提问期间一直成立）。
    """

    def test_second_create_refreshes_instead_of_inserting(
        self, service: WakeService
    ) -> None:
        first = _session_trigger(service, times=2)
        await_second = _session_trigger(service, times=2)
        assert await_second.id == first.id
        assert len(service.list_triggers()) == 1
        assert await_second.remaining_done == 2

    @pytest.mark.asyncio
    async def test_refresh_resets_the_quota(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        """刷新是"下一次完成时告诉我"：用掉的配额被重置回本次请求的值。"""
        first = _session_trigger(service, times=3)
        await service._dispatch_one(first, EVENT_DONE, _done())
        assert service.list_triggers()[0].remaining_done == 2

        refreshed = _session_trigger(service, times=3)
        assert refreshed.id == first.id
        assert refreshed.remaining_done == 3
        assert len(service.list_triggers()) == 1

    def test_different_on_events_is_a_different_subscription(
        self, service: WakeService
    ) -> None:
        both = _session_trigger(service)
        only_ask = _session_trigger(service, on_events="ask")
        assert only_ask.id != both.id
        assert len(service.list_triggers()) == 2

    def test_different_target_is_a_different_subscription(
        self, service: WakeService
    ) -> None:
        _session_trigger(service)
        other = _session_trigger(service, target_session_id=OTHER)
        assert len(service.list_triggers()) == 2
        assert other.target_session_id == OTHER

    def test_finished_subscription_is_not_refreshed(self, service: WakeService) -> None:
        """终结的那条不再是"下一次完成时告诉我"：再登记会得到一条新行。"""
        first = _session_trigger(service)
        service.cancel(first.id)
        second = _session_trigger(service)
        assert second.id != first.id
        assert len(service.list_triggers()) == 1

    def test_refreshed_prompt_wins(self, service: WakeService) -> None:
        _session_trigger(service, prompt="old text")
        refreshed = _session_trigger(service, prompt="new text")
        assert refreshed.prompt == "new text"
        assert len(service.list_triggers()) == 1


# ============================================================
# cancel / list
# ============================================================


class TestCancelAndList:
    def test_cancel_is_idempotent(self, service: WakeService) -> None:
        trigger = _session_trigger(service)
        assert service.cancel(trigger.id) is True
        assert service.cancel(trigger.id) is False
        assert service.list_triggers() == []
        assert [t.state for t in service.list_triggers(include_finished=True)] == [
            STATE_CANCELED
        ]

    def test_cancel_unknown_id_raises(self, service: WakeService) -> None:
        with pytest.raises(LookupError):
            service.cancel(999)

    def test_cancel_for_session_matches_both_sides(self, service: WakeService) -> None:
        as_source = _session_trigger(service)
        as_target = _session_trigger(
            service, source_session_id=OTHER, target_session_id=CHILD
        )
        unrelated = _session_trigger(service, source_session_id=OTHER)
        assert service.cancel_for_session(CHILD) == 2
        states = {t.id: t.state for t in service.list_triggers(include_finished=True)}
        assert states[as_source.id] == STATE_CANCELED
        assert states[as_target.id] == STATE_CANCELED
        assert states[unrelated.id] == STATE_ACTIVE


# ============================================================
# 事件分发
# ============================================================


class TestEventDispatch:
    @pytest.mark.asyncio
    async def test_done_wakes_and_consumes_the_quota(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        _session_trigger(service)
        service._on_event(_done())
        await _drain()

        assert len(ports.delivered) == 1
        target, content, request_id = ports.delivered[0]
        assert target == PARENT
        assert "[wing] session" in content and "finished" in content
        assert "subtype=success" in content and "turns=2" in content
        assert "1.5s" in content and "all good" in content
        assert request_id and len(request_id) == 32  # uuid4().hex
        assert service.list_triggers(include_finished=True)[0].state == STATE_DONE
        assert service.list_triggers(include_finished=True)[0].fire_count == 1
        # done 送达即终结：不再留 ask 空转
        assert service.list_triggers() == []

    @pytest.mark.asyncio
    async def test_times_quota_counts_down(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        trigger = _session_trigger(service, times=2)
        service._on_event(_done())
        await _drain()
        after_first = service.list_triggers(include_finished=True)[0]
        assert after_first.state == STATE_ACTIVE
        assert after_first.remaining_done == 1

        service._on_event(_done())
        await _drain()
        assert len(ports.delivered) == 2
        assert service.list_triggers(include_finished=True)[0].state == STATE_DONE
        assert service.list_triggers(include_finished=True)[0].id == trigger.id

    @pytest.mark.asyncio
    async def test_ask_does_not_consume_the_quota(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        _session_trigger(service)
        service._on_event(_ask())
        await _drain()

        assert len(ports.delivered) == 1
        content = ports.delivered[0][1]
        assert "waiting for your answer" in content
        assert "tool_call_id=tc-1" in content
        assert "- [Color] Which?  (red / blue)" in content
        trigger = service.list_triggers(include_finished=True)[0]
        assert trigger.state == STATE_ACTIVE
        assert trigger.remaining_done == 1
        assert trigger.fire_count == 1

    @pytest.mark.asyncio
    async def test_on_events_filters(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        _session_trigger(service, on_events="ask")
        service._on_event(_done())
        await _drain()
        assert ports.delivered == []
        service._on_event(_ask())
        await _drain()
        assert len(ports.delivered) == 1

    @pytest.mark.asyncio
    async def test_custom_prompt_is_sent_verbatim(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        _session_trigger(service, prompt="child finished, take a look")
        service._on_event(_done())
        await _drain()
        assert ports.delivered[0][1] == "child finished, take a look"

    @pytest.mark.asyncio
    async def test_every_trigger_on_the_same_source_fires(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        first = _session_trigger(service)
        second = _session_trigger(service, target_session_id=OTHER)
        service._on_event(_done())
        await _drain()
        assert [target for target, _, _ in ports.delivered] == [PARENT, OTHER]
        states = {t.id: t.state for t in service.list_triggers(include_finished=True)}
        assert states[first.id] == states[second.id] == STATE_DONE

    @pytest.mark.asyncio
    async def test_events_of_other_sessions_are_ignored(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        _session_trigger(service)
        service._on_event(_done(session_id=OTHER))
        await _drain()
        assert ports.delivered == []

    @pytest.mark.asyncio
    async def test_canceled_trigger_does_not_fire(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        trigger = _session_trigger(service)
        service.cancel(trigger.id)
        service._on_event(_done())
        await _drain()
        assert ports.delivered == []

    def test_no_running_loop_only_logs(self, service: WakeService) -> None:
        """没有运行中的事件循环：只记日志，绝不抛（emit 栈里不能炸）。"""
        _session_trigger(service)
        service._on_event(_done())  # 同步上下文调用：不抛

    def test_unrelated_events_are_ignored(self, service: WakeService) -> None:
        _session_trigger(service)
        service._on_event(DoneEvent(session_id=CHILD))


# ============================================================
# 投递失败
# ============================================================


class TestBookkeepingFailure:
    """``wing.db`` 的写是**可能静默失败**的（磁盘满 / 只读 / 长锁）：记账丢了怎么办。"""

    @pytest.mark.asyncio
    async def test_lost_write_stalls_the_timer_instead_of_reposting(
        self,
        service: WakeService,
        ports: FakePorts,
        clock: Clock,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """到点的 timer 记账失败 → 本进程不再重试（否则每 10ms 重投一次）。"""
        trigger = _timer_trigger(service, spec="in:30s")
        clock.now = trigger.next_fire_at_ms or 0
        monkeypatch.setattr(service._store, "record_fire", lambda *a, **k: False)

        await service.fire_timer(trigger, clock.now)
        assert len(ports.delivered) == 1  # 这一次投递已经发生了

        # 库里它仍是 active + 已过期，但调度器再也看不到它（stalled）
        assert service.due_timers(clock.now + 10**6) == []
        assert service.next_due_ms() is None

    @pytest.mark.asyncio
    async def test_lost_state_write_on_failure_also_stalls(
        self,
        service: WakeService,
        ports: FakePorts,
        clock: Clock,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """投递失败后连 ``state=failed`` 都写不进去：同样撤出重试。"""
        trigger = _timer_trigger(service, spec="in:30s")
        clock.now = trigger.next_fire_at_ms or 0
        ports.error = LookupError("gone")
        monkeypatch.setattr(service._store, "set_state", lambda *a, **k: False)

        await service.fire_timer(trigger, clock.now)
        assert service.due_timers(clock.now + 10**6) == []

    @pytest.mark.asyncio
    async def test_stalled_session_trigger_stops_waking(
        self,
        service: WakeService,
        ports: FakePorts,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        trigger = _session_trigger(service, times=2)
        monkeypatch.setattr(service._store, "record_fire", lambda *a, **k: False)
        service._on_event(_done())
        await _drain()
        assert len(ports.delivered) == 1
        assert service.list_triggers(include_finished=True)[0].id == trigger.id

        # 同一条触发器后来的事件不再唤醒（记账已经丢了，不能当作它还有配额）
        service._on_event(_done())
        await _drain()
        assert len(ports.delivered) == 1


class TestCancelWriteLost:
    """取消时的"写丢失"必须是一种**独立的结局**，不能说成"它早完了"。"""

    def test_cancel_write_lost_raises_unavailable(
        self,
        service: WakeService,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        trigger = _session_trigger(service)
        database = service._store.database()
        real_execute = database.execute

        def flaky(sql: str, params=()):
            if sql.strip().startswith("UPDATE wake_trigger"):
                return False
            return real_execute(sql, params)

        monkeypatch.setattr(database, "execute", flaky)
        with pytest.raises(WakeUnavailableError, match="write lost"):
            service.cancel(trigger.id)
        # 库里那行仍在武装（这正是不能对用户说"已终结"的理由）
        assert [t.id for t in service.list_triggers()] == [trigger.id]

    def test_cancel_for_session_never_lies_about_the_count(
        self, service: WakeService, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _session_trigger(service)
        database = service._store.database()
        monkeypatch.setattr(database, "executemany", lambda sql, rows: False)
        assert service.cancel_for_session(CHILD) == 0  # 一条都没写成
        assert len(service.list_triggers()) == 1


class TestCancelOnDegradedDatabase:
    def test_cancel_reports_unavailable_not_missing(
        self, ports: FakePorts, tmp_path: Path
    ) -> None:
        """降级库里"查不到"与"库坏了"同形，但对用户是两种结论（404 vs 503）。"""
        path = tmp_path / "wing.db"
        path.mkdir()
        service = WakeService(ports, database=Database(path))
        service.install()
        with pytest.raises(WakeUnavailableError):
            service.cancel(1)


class TestRecoverySkipsStalled:
    @pytest.mark.asyncio
    async def test_a_stalled_trigger_is_not_compensated_on_restart_of_the_loop(
        self,
        service: WakeService,
        ports: FakePorts,
        clock: Clock,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """`_stalled` 必须也拦得住 ``recover_missed``：否则 stop()/start() 一次就再投一次。"""
        trigger = _timer_trigger(service, spec="in:30s")
        clock.now = trigger.next_fire_at_ms or 0
        monkeypatch.setattr(service._store, "record_fire", lambda *a, **k: False)
        await service.fire_timer(trigger, clock.now)
        assert len(ports.delivered) == 1

        assert await service.recover_missed() == 0
        assert len(ports.delivered) == 1


class TestConcurrentDispatch:
    @pytest.mark.asyncio
    async def test_stale_snapshots_do_not_lose_a_decrement(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        """两次分发都用**同一份旧快照**：配额必须被扣两次（不能后写覆盖先写）。"""
        trigger = _session_trigger(service, times=2)
        stale = trigger  # 快照里 remaining_done == 2
        await service._dispatch_one(stale, EVENT_DONE, _done())
        await service._dispatch_one(stale, EVENT_DONE, _done())

        row = service.list_triggers(include_finished=True)[0]
        assert row.id == trigger.id
        assert row.fire_count == 2
        assert row.remaining_done == 0
        assert row.state == STATE_DONE


class TestDeliveryFailure:
    @pytest.mark.asyncio
    async def test_failure_marks_failed_and_never_raises(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        _session_trigger(service)
        ports.error = LookupError("Session not found: 'parent'")
        service._on_event(_done())
        await _drain()
        trigger = service.list_triggers(include_finished=True)[0]
        assert trigger.state == STATE_FAILED
        assert "Session not found" in (trigger.last_error or "")
        assert service.list_triggers() == []  # 终态不再武装

    @pytest.mark.asyncio
    async def test_archived_target_is_named(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        _session_trigger(service)
        ports.error = LookupError("gone")
        ports.archived.add(PARENT)
        service._on_event(_done())
        await _drain()
        error = service.list_triggers(include_finished=True)[0].last_error or ""
        assert "archived" in error and PARENT in error

    @pytest.mark.asyncio
    async def test_one_failure_does_not_block_the_others(
        self, service: WakeService, ports: FakePorts, db: Database
    ) -> None:
        """两条触发器指向不同 target：第一条失败不影响第二条。"""
        _session_trigger(service)

        class FlakyPorts(FakePorts):
            async def deliver(self, *, session_id, content, request_id):
                if session_id == PARENT:
                    raise RuntimeError("boom")
                await super().deliver(
                    session_id=session_id, content=content, request_id=request_id
                )

        flaky = FlakyPorts()
        flaky.existing = set(ports.existing)
        service2 = WakeService(flaky, database=db, clock=Clock())
        service2.install()
        _session_trigger(service2, target_session_id=OTHER)
        service2._on_event(_done())
        await _drain()
        assert [target for target, _, _ in flaky.delivered] == [OTHER]


# ============================================================
# 定时触发
# ============================================================


class TestTimerFire:
    @pytest.mark.asyncio
    async def test_envelope_and_next_fire(
        self, service: WakeService, ports: FakePorts, clock: Clock
    ) -> None:
        trigger = _timer_trigger(service, spec="every:30m")
        due = trigger.next_fire_at_ms or 0
        clock.now = due
        await service.fire_timer(trigger, clock.now)

        assert ports.delivered[0][1] == "[wing] scheduled wake (every:30m, #1)\n\nping"
        fired = service.list_triggers(include_finished=True)[0]
        assert fired.state == STATE_ACTIVE
        assert fired.fire_count == 1
        assert fired.last_fire_at_ms == due
        assert fired.next_fire_at_ms == due + 1_800_000  # 从 now 起算

    @pytest.mark.asyncio
    async def test_budget_exhaustion_finishes(
        self, service: WakeService, ports: FakePorts, clock: Clock
    ) -> None:
        trigger = _timer_trigger(service, spec="every:1m", times=2)
        clock.now = trigger.next_fire_at_ms or 0
        await service.fire_timer(trigger, clock.now)
        assert service.list_triggers()[0].remaining_done == 1
        again = service.list_triggers()[0]
        await service.fire_timer(again, clock.now)
        finished = service.list_triggers(include_finished=True)[0]
        assert finished.state == STATE_DONE
        assert finished.next_fire_at_ms is None
        assert [c.splitlines()[0] for _, c, _ in ports.delivered] == [
            "[wing] scheduled wake (every:1m, #1)",
            "[wing] scheduled wake (every:1m, #2)",
        ]

    @pytest.mark.asyncio
    async def test_one_shot_finishes_after_one_fire(
        self, service: WakeService, ports: FakePorts, clock: Clock
    ) -> None:
        trigger = _timer_trigger(service, spec="in:30s")
        clock.now = trigger.next_fire_at_ms or 0
        await service.fire_timer(trigger, clock.now)
        fired = service.list_triggers(include_finished=True)[0]
        assert fired.state == STATE_DONE
        assert fired.remaining_done == 0
        assert fired.next_fire_at_ms is None

    @pytest.mark.asyncio
    async def test_delivery_failure_keeps_it_out_of_the_budget(
        self, service: WakeService, ports: FakePorts, clock: Clock
    ) -> None:
        trigger = _timer_trigger(service, spec="in:30s")
        ports.error = LookupError("gone")
        clock.now = trigger.next_fire_at_ms or 0
        await service.fire_timer(trigger, clock.now)
        failed = service.list_triggers(include_finished=True)[0]
        assert failed.state == STATE_FAILED
        assert failed.fire_count == 0  # 失败不算一次触发


# ============================================================
# 重启恢复
# ============================================================


class TestRecovery:
    @pytest.mark.asyncio
    async def test_missed_one_shot_is_compensated_once(
        self, service: WakeService, ports: FakePorts, clock: Clock
    ) -> None:
        trigger = _timer_trigger(service, spec="in:5s")
        clock.advance(120)  # 网关停机两分钟：早就该触发了

        assert await service.recover_missed() == 1
        assert len(ports.delivered) == 1
        finished = service.list_triggers(include_finished=True)[0]
        assert finished.id == trigger.id
        assert finished.state == STATE_DONE
        assert finished.fire_count == 1

        # 绝不会追债式连发：再跑一次恢复什么都不做
        assert await service.recover_missed() == 0
        assert len(ports.delivered) == 1

    @pytest.mark.asyncio
    async def test_missed_recurring_realigns_without_firing(
        self, service: WakeService, ports: FakePorts, clock: Clock
    ) -> None:
        trigger = _timer_trigger(service, spec="every:30m")
        clock.advance(3 * 3600)  # 错过 6 个 tick

        assert await service.recover_missed() == 1
        assert ports.delivered == []  # 不补发
        realigned = service.list_triggers()[0]
        assert realigned.id == trigger.id
        assert realigned.state == STATE_ACTIVE
        assert realigned.fire_count == 0
        assert realigned.next_fire_at_ms == clock.now + 1_800_000

    @pytest.mark.asyncio
    async def test_future_triggers_are_untouched(
        self, service: WakeService, ports: FakePorts
    ) -> None:
        trigger = _timer_trigger(service, spec="every:30m")
        assert await service.recover_missed() == 0
        assert service.list_triggers()[0].next_fire_at_ms == trigger.next_fire_at_ms


# ============================================================
# DB 不可用
# ============================================================


class TestDatabaseUnavailable:
    @pytest.fixture
    def degraded(self, tmp_path: Path) -> Database:
        """库路径被占成目录：sqlite3 打不开 → 降级对象。"""
        path = tmp_path / "wing.db"
        path.mkdir()
        database = Database(path)
        assert not database.ready
        return database

    def test_install_disarms_without_raising(
        self, ports: FakePorts, degraded: Database
    ) -> None:
        service = WakeService(ports, database=degraded)
        service.install()
        assert event_bus.subscriber_count == 0  # 订阅都没装（整体不武装）
        service.start()  # 未武装：start 是 no-op（也不抛）
        assert service.list_triggers() == []
        assert service.due_timers(10**12) == []
        assert service.next_due_ms() is None

    @pytest.mark.asyncio
    async def test_stop_is_safe(self, ports: FakePorts, degraded: Database) -> None:
        service = WakeService(ports, database=degraded)
        await service.stop()

    def test_create_reports_unavailable(
        self, ports: FakePorts, degraded: Database
    ) -> None:
        service = WakeService(ports, database=degraded)
        service.install()
        with pytest.raises(WakeUnavailableError):
            service.create(
                kind="session",
                target_session_id=PARENT,
                source_session_id=CHILD,
            )
