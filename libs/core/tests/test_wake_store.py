"""tests/test_wake_store.py — ``wake_trigger`` 的 CRUD 与排序语义。

服务层测试已覆盖业务路径；这里只钉存储本身的形状：插入读回（id 走 ``last_insert_rowid()``）、
列表顺序与过滤、到期队列的定序（同一 tick 内顺序确定）、降级库的空操作。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.db.database import Database
from wing.wake.model import STATE_ACTIVE, STATE_CANCELED, STATE_DONE, STATE_FAILED
from wing.wake.store import WakeStore


@pytest.fixture
def store(tmp_path: Path) -> WakeStore:
    database = Database(tmp_path / "wing.db")
    assert database.ready
    return WakeStore(database)


def _insert(store: WakeStore, **overrides):
    payload = {
        "kind": "session",
        "target_session_id": "parent",
        "source_session_id": "child",
        "on_events": "done,ask",
        "spec": None,
        "next_fire_at_ms": None,
        "remaining_done": 1,
        "prompt": None,
        "created_at_ms": 100,
        "created_by": "test",
    }
    payload.update(overrides)
    trigger = store.insert(**payload)
    assert trigger is not None
    return trigger


def _insert_timer(store: WakeStore, next_fire_at_ms: int, **overrides):
    return _insert(
        store,
        kind="timer",
        source_session_id=None,
        spec="every:30m",
        next_fire_at_ms=next_fire_at_ms,
        prompt="ping",
        **overrides,
    )


class TestInsert:
    def test_round_trips_every_column(self, store: WakeStore) -> None:
        created = _insert(
            store,
            kind="timer",
            source_session_id=None,
            spec="in:30s",
            next_fire_at_ms=999,
            remaining_done=None,
            prompt="ping",
            created_by="cli",
        )
        loaded = store.get(created.id)
        assert loaded == created
        assert loaded is not None
        assert loaded.id == created.id
        assert loaded.state == STATE_ACTIVE
        assert loaded.fire_count == 0
        assert loaded.created_by == "cli"

    def test_ids_are_monotonic(self, store: WakeStore) -> None:
        first = _insert(store)
        second = _insert(store)
        assert second.id == first.id + 1


class TestQueries:
    def test_list_is_newest_first_and_filters_finished(self, store: WakeStore) -> None:
        first = _insert(store)
        second = _insert(store)
        store.set_state(first.id, STATE_CANCELED)
        assert [t.id for t in store.list_triggers()] == [second.id]
        assert [t.id for t in store.list_triggers(include_finished=True)] == [
            second.id,
            first.id,
        ]

    def test_active_for_source_is_oldest_first(self, store: WakeStore) -> None:
        first = _insert(store)
        second = _insert(store)
        store.set_state(first.id, STATE_CANCELED)
        assert [t.id for t in store.active_for_source("child")] == [second.id]
        assert store.active_for_source("ghost") == []

    def test_due_timers_orders_by_deadline_then_id(self, store: WakeStore) -> None:
        late = _insert_timer(store, 300)
        early_a = _insert_timer(store, 100)
        early_b = _insert_timer(store, 100)
        future = _insert_timer(store, 10_000)
        due = store.due_timers(500)
        assert [t.id for t in due] == [early_a.id, early_b.id, late.id]
        assert future.id not in {t.id for t in due}

    def test_due_timers_ignores_finished_and_session_triggers(
        self, store: WakeStore
    ) -> None:
        canceled = _insert_timer(store, 100)
        store.set_state(canceled.id, STATE_CANCELED)
        session_trigger = _insert(store, next_fire_at_ms=50)
        assert store.due_timers(500) == []
        assert session_trigger.id  # 只是确认它存在且被过滤掉

    def test_next_due_ms_is_the_minimum(self, store: WakeStore) -> None:
        assert store.next_due_ms() is None
        _insert_timer(store, 500)
        _insert_timer(store, 200)
        assert store.next_due_ms() == 200

    def test_next_due_ms_ignores_finished(self, store: WakeStore) -> None:
        trigger = _insert_timer(store, 200)
        store.set_state(trigger.id, STATE_CANCELED)
        assert store.next_due_ms() is None


class TestMutations:
    def test_record_fire_increments_and_reschedules(self, store: WakeStore) -> None:
        trigger = _insert_timer(store, 100, remaining_done=4)
        assert store.record_fire(
            trigger.id, fired_at_ms=120, decrement=1, next_fire_at_ms=200
        )
        loaded = store.get(trigger.id)
        assert loaded is not None
        assert (loaded.fire_count, loaded.last_fire_at_ms) == (1, 120)
        assert (loaded.next_fire_at_ms, loaded.remaining_done) == (200, 3)

    def test_record_fire_exhausts_the_budget_atomically(self, store: WakeStore) -> None:
        """预算扣到 0 与"清排期 + 终结"是同一条语句（并发下也不会丢一次扣减）。"""
        trigger = _insert_timer(store, 100, remaining_done=1)
        assert store.record_fire(
            trigger.id, fired_at_ms=120, decrement=1, next_fire_at_ms=999
        )
        loaded = store.get(trigger.id)
        assert loaded is not None
        assert loaded.state == STATE_DONE
        assert loaded.remaining_done == 0
        assert loaded.next_fire_at_ms is None, "终态不该留下排期"

    def test_record_fire_keeps_unlimited_budgets_unlimited(
        self, store: WakeStore
    ) -> None:
        trigger = _insert_timer(store, 100, remaining_done=None)
        assert store.record_fire(
            trigger.id, fired_at_ms=120, decrement=1, next_fire_at_ms=200
        )
        loaded = store.get(trigger.id)
        assert loaded is not None
        assert loaded.remaining_done is None
        assert loaded.state == STATE_ACTIVE
        assert loaded.next_fire_at_ms == 200

    def test_record_fire_is_a_noop_on_finished_rows(self, store: WakeStore) -> None:
        trigger = _insert(store)
        store.set_state(trigger.id, STATE_CANCELED)
        assert store.record_fire(
            trigger.id, fired_at_ms=1, decrement=1, next_fire_at_ms=None
        )
        loaded = store.get(trigger.id)
        assert loaded is not None and loaded.fire_count == 0

    def test_set_state_failed_records_error(self, store: WakeStore) -> None:
        trigger = _insert(store)
        store.set_state(trigger.id, STATE_FAILED, error="boom")
        loaded = store.get(trigger.id)
        assert loaded is not None
        assert loaded.state == STATE_FAILED
        assert loaded.last_error == "boom"

    def test_cancel_semantics(self, store: WakeStore) -> None:
        trigger = _insert(store)
        assert store.cancel(trigger.id) is True
        assert store.cancel(trigger.id) is False  # 幂等 no-op
        assert store.cancel(999) is None  # 不存在（路由据此 404）

    def test_cancel_for_session_counts(self, store: WakeStore) -> None:
        as_source = _insert(store)
        as_target = _insert(store, source_session_id="other", target_session_id="child")
        assert store.cancel_for_session("child") == 2
        source = store.get(as_source.id)
        target = store.get(as_target.id)
        assert source is not None and source.state == STATE_CANCELED
        assert target is not None and target.state == STATE_CANCELED
        assert store.cancel_for_session("child") == 0  # 重复调用不再计数


class TestDegradedDatabase:
    @pytest.fixture
    def degraded_store(self, tmp_path: Path) -> WakeStore:
        path = tmp_path / "wing.db"
        path.mkdir()
        return WakeStore(Database(path))

    def test_writes_are_noops_and_reads_are_empty(
        self, degraded_store: WakeStore
    ) -> None:
        assert (
            degraded_store.insert(
                kind="session",
                target_session_id="parent",
                source_session_id="child",
                on_events="done,ask",
                spec=None,
                next_fire_at_ms=None,
                remaining_done=1,
                prompt=None,
                created_at_ms=0,
                created_by=None,
            )
            is None
        )
        assert degraded_store.list_triggers() == []
        assert degraded_store.next_due_ms() is None
        assert degraded_store.due_timers(10**12) == []
        assert degraded_store.cancel_for_session("child") == 0
        degraded_store.set_state(1, STATE_FAILED, error="x")  # 不抛
