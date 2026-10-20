"""tests/test_wake_scheduler.py — 调度循环：到期立即触发、notify 打断、停止幂等。

时钟全部注入（推进假时钟），等待只有「让出事件循环」与有界 ``wait_for`` 上限——没有按时间
sleep 的断言。守的两条坑：

  - ``asyncio.wait_for(timeout<=0)`` 走特例分支**立即超时**（``background.py`` 的同款坑），
    所以「现在到期」必须把等待时长抬到 ``_MIN_TICK_SECONDS``；
  - 单个触发失败不影响同一 tick 的其它触发器。
"""

from __future__ import annotations

import asyncio

import pytest

from wing.wake.model import STATE_ACTIVE, WakeTrigger
from wing.wake.scheduler import WakeScheduler


class Clock:
    def __init__(self, now_ms: int = 1_800_000_000_000) -> None:
        self.now = now_ms

    def __call__(self) -> int:
        return self.now


def _trigger(trigger_id: int, fire_at_ms: int) -> WakeTrigger:
    return WakeTrigger(
        id=trigger_id,
        kind="timer",
        target_session_id="parent",
        source_session_id=None,
        on_events="done,ask",
        spec="in:30s",
        next_fire_at_ms=fire_at_ms,
        remaining_done=1,
        prompt="ping",
        state=STATE_ACTIVE,
        fire_count=0,
        last_fire_at_ms=None,
        last_error=None,
        created_at_ms=0,
        created_by=None,
    )


class FakeSource:
    """``TriggerSource`` 替身：排期表 + 触发记录 + 可注入失败。"""

    def __init__(self, clock: Clock) -> None:
        self.clock = clock
        self.pending: list[WakeTrigger] = []
        self.fired: list[int] = []
        self.recovered = 0
        self.fail_ids: set[int] = set()
        self.fired_event = asyncio.Event()

    async def recover_missed(self) -> int:
        self.recovered += 1
        return 0

    def due_timers(self, now_ms: int) -> list[WakeTrigger]:
        return [t for t in self.pending if (t.next_fire_at_ms or 0) <= now_ms]

    async def fire_timer(self, trigger: WakeTrigger, now_ms: int) -> None:
        if trigger.id in self.fail_ids:
            raise RuntimeError(f"trigger {trigger.id} exploded")
        self.fired.append(trigger.id)
        self.pending = [t for t in self.pending if t.id != trigger.id]
        self.fired_event.set()

    def next_due_ms(self) -> int | None:
        values = [t.next_fire_at_ms for t in self.pending]
        values = [value for value in values if value is not None]
        return min(values) if values else None


@pytest.fixture
def clock() -> Clock:
    return Clock()


@pytest.fixture
def source(clock: Clock) -> FakeSource:
    return FakeSource(clock)


@pytest.fixture
def scheduler(source: FakeSource, clock: Clock) -> WakeScheduler:
    return WakeScheduler(source, clock=clock)


class TestTimeout:
    def test_due_now_is_clamped_to_the_min_tick(
        self, scheduler: WakeScheduler, source: FakeSource, clock: Clock
    ) -> None:
        """已经到期 → 等待时长是**下限**，不是 0（0 会让 wait_for 立即超时并跳过停止检查）。"""
        source.pending.append(_trigger(1, clock.now - 5000))
        assert scheduler.next_timeout_seconds() == scheduler._MIN_TICK_SECONDS

    def test_future_deadline_is_the_remaining_seconds(
        self, scheduler: WakeScheduler, source: FakeSource, clock: Clock
    ) -> None:
        source.pending.append(_trigger(1, clock.now + 30_000))
        assert scheduler.next_timeout_seconds() == pytest.approx(30.0)

    def test_no_timers_waits_forever(self, scheduler: WakeScheduler) -> None:
        assert scheduler.next_timeout_seconds() is None


class TestFireDue:
    @pytest.mark.asyncio
    async def test_fires_every_due_trigger(
        self, scheduler: WakeScheduler, source: FakeSource, clock: Clock
    ) -> None:
        source.pending = [
            _trigger(1, clock.now - 1000),
            _trigger(2, clock.now - 500),
            _trigger(3, clock.now + 60_000),  # 未到期
        ]
        assert await scheduler.fire_due() == 2
        assert source.fired == [1, 2]

    @pytest.mark.asyncio
    async def test_one_failure_does_not_block_the_rest(
        self, scheduler: WakeScheduler, source: FakeSource, clock: Clock
    ) -> None:
        source.fail_ids.add(1)
        source.pending = [_trigger(1, clock.now - 1000), _trigger(2, clock.now - 500)]
        assert await scheduler.fire_due() == 2
        assert source.fired == [2]


class TestLoop:
    @pytest.mark.asyncio
    async def test_start_runs_recovery_first(
        self, scheduler: WakeScheduler, source: FakeSource
    ) -> None:
        scheduler.start()
        await asyncio.wait_for(self._until(lambda: source.recovered == 1), timeout=2)
        await scheduler.stop()

    @pytest.mark.asyncio
    async def test_notify_fires_a_newly_due_trigger(
        self, scheduler: WakeScheduler, source: FakeSource, clock: Clock
    ) -> None:
        """没有定时器时循环无限等；``notify()`` 打断后重算，到期的立刻触发。"""
        scheduler.start()
        await asyncio.wait_for(self._until(lambda: source.recovered == 1), timeout=2)

        source.pending.append(_trigger(1, clock.now))
        scheduler.notify()
        await asyncio.wait_for(source.fired_event.wait(), timeout=2)
        assert source.fired == [1]
        await scheduler.stop()

    @pytest.mark.asyncio
    async def test_timeout_path_fires_without_notify(
        self, scheduler: WakeScheduler, source: FakeSource, clock: Clock
    ) -> None:
        """不经 notify 的路径：到点（假时钟已越过）由 wait_for 超时驱动触发。"""
        source.pending.append(_trigger(1, clock.now + 1))
        scheduler.start()
        clock.now += 60_000
        # 循环在启动时算出的 timeout 很短（1ms 下限之上），超时后重算即触发
        await asyncio.wait_for(source.fired_event.wait(), timeout=2)
        assert source.fired == [1]
        await scheduler.stop()

    @pytest.mark.asyncio
    async def test_stop_is_idempotent(
        self, scheduler: WakeScheduler, source: FakeSource
    ) -> None:
        scheduler.start()
        await scheduler.stop()
        await scheduler.stop()
        assert scheduler.running is False

    @pytest.mark.asyncio
    async def test_stop_without_start_is_a_noop(self, scheduler: WakeScheduler) -> None:
        await scheduler.stop()

    @pytest.mark.asyncio
    async def test_stop_interrupts_an_infinite_wait(
        self, scheduler: WakeScheduler, source: FakeSource
    ) -> None:
        scheduler.start()
        await asyncio.wait_for(self._until(lambda: source.recovered == 1), timeout=2)
        await asyncio.wait_for(scheduler.stop(), timeout=2)
        assert scheduler.running is False

    @staticmethod
    async def _until(predicate) -> None:
        while not predicate():
            await asyncio.sleep(0)
