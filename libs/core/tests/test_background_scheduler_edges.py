"""BackgroundScheduler：幂等、无人认领时的等待、循环异常与取消语义。

调度器是周期任务的宿主（首个 job 是空闲会话逐出），它的 stop 语义直接决定
gateway 关闭时会不会挂住或假装正常返回。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from _support import wait_until_async
from wing.background import BackgroundScheduler

pytestmark = pytest.mark.asyncio


async def test_start_is_idempotent():
    scheduler = BackgroundScheduler()
    scheduler.start()
    first = scheduler._task
    scheduler.start()

    assert scheduler.running is True
    assert scheduler._task is first
    await scheduler.stop()


async def test_stop_without_start_returns():
    scheduler = BackgroundScheduler()

    await scheduler.stop()

    assert scheduler.running is False


async def test_empty_scheduler_waits_for_stop():
    """没有 job 时不空转：循环阻塞在 stop 事件上。"""
    scheduler = BackgroundScheduler()
    scheduler.start()
    await asyncio.sleep(0)  # 让循环跑起来
    assert scheduler._task is not None and not scheduler._task.done()

    await scheduler.stop()

    assert scheduler.running is False


async def test_job_not_due_is_skipped():
    scheduler = BackgroundScheduler()
    ran: list[int] = []
    scheduler.add_job("later", 3600, lambda: ran.append(1))
    scheduler.start()
    # 这个 job 3600s 后才到期，循环只会挂起；等 20ms 是给「等待被算错」留的窗口——
    # 若 `_run` 退化成忙循环（wait ≤ 0），到期判定会在这一窗口里反复执行。
    await asyncio.sleep(0.02)

    await scheduler.stop()

    assert ran == []


async def test_due_job_runs_and_reschedules():
    scheduler = BackgroundScheduler()
    ran: list[int] = []
    scheduler.add_job("tick", 0.02, lambda: ran.append(1))
    scheduler.start()

    await wait_until_async(lambda: len(ran) >= 2, timeout=5.0)  # 多轮触发
    await scheduler.stop()

    assert len(ran) >= 2  # 跑完先排下一次，不忙循环
    assert scheduler.jobs == ["tick"]


async def test_failing_job_does_not_kill_loop(wing_warnings):
    """job 抛异常只记日志：循环继续，其它 job 不受影响。"""
    scheduler = BackgroundScheduler()
    healthy: list[int] = []

    def boom() -> None:
        raise RuntimeError("job exploded")

    scheduler.add_job("boom", 0.02, boom)
    scheduler.add_job("ok", 0.02, lambda: healthy.append(1))
    scheduler.start()

    await wait_until_async(
        lambda: bool(healthy) and any("job 'boom' failed" in m for m in wing_warnings),
        timeout=5.0,
    )
    await scheduler.stop()

    assert healthy  # 健康 job 照常跑
    assert any("job 'boom' failed" in m for m in wing_warnings)


async def test_stop_waits_for_running_job():
    """stop 等正在执行的 job 跑完（不再触发后续轮次）。"""
    scheduler = BackgroundScheduler()
    finished: list[str] = []
    entered = asyncio.Event()
    release = asyncio.Event()

    async def slow() -> None:
        entered.set()
        await release.wait()
        finished.append("done")

    scheduler.add_job("slow", 0.01, slow)
    scheduler.start()
    await asyncio.wait_for(entered.wait(), timeout=5.0)  # job 已在执行

    stop_task = asyncio.create_task(scheduler.stop())
    await asyncio.sleep(0)  # 让 stop 跑到「等循环任务」处
    assert not stop_task.done(), "job 还在跑，stop 不得返回"

    release.set()
    await asyncio.wait_for(stop_task, timeout=5.0)

    assert finished == ["done"]


async def test_stop_swallows_loop_cancellation(wing_warnings):
    """循环自身被取消（事件循环收尾）→ stop 吞掉，不算错误。"""
    scheduler = BackgroundScheduler()
    scheduler.add_job("tick", 0.02, lambda: None)
    scheduler.start()
    task = scheduler._task
    assert task is not None
    task.cancel()

    await scheduler.stop()


async def test_stop_swallows_outer_cancellation_when_loop_cancelled():
    """外层取消（含 wait_for 超时）会同时取消循环任务——两者都收，不向上抛。"""
    scheduler = BackgroundScheduler()
    scheduler.start()
    stop_task = asyncio.create_task(scheduler.stop())
    await asyncio.sleep(0.01)

    stop_task.cancel()

    # 循环任务确实被取消（不是"循环还在跑"），因此 stop 吸收这个取消
    await stop_task
    assert scheduler.running is False


async def test_stop_logs_loop_death(wing_warnings):
    """循环自身异常也从 stop 里吸收（只记 error）。"""
    scheduler = BackgroundScheduler()

    async def dying() -> None:
        raise RuntimeError("loop died")

    scheduler._task = asyncio.create_task(dying())
    await asyncio.sleep(0)

    await scheduler.stop()

    assert any("loop died with error" in m for m in wing_warnings)
    assert scheduler.running is False


async def test_add_job_validation():
    scheduler = BackgroundScheduler()
    scheduler.add_job("a", 1, lambda: None)

    with pytest.raises(ValueError, match="interval must be > 0"):
        scheduler.add_job("b", 0, lambda: None)
    with pytest.raises(ValueError, match="already registered"):
        scheduler.add_job("a", 1, lambda: None)
    with pytest.raises(ValueError, match="initial delay must be >= 0"):
        scheduler.add_job("c", 1, lambda: None, initial_delay_seconds=-1)
    scheduler.remove_job("a")
    scheduler.remove_job("missing")  # 未知名称静默忽略
    assert scheduler.jobs == []


async def test_initial_delay_zero_runs_immediately():
    """``initial_delay_seconds=0`` = 下一个 tick 立即跑首轮（不等 interval），
    之后仍按 interval 周期——"启动过一会先扫一轮"的机制（自动归档用）。"""
    scheduler = BackgroundScheduler()
    ran: list[int] = []
    scheduler.add_job("now", 3600, lambda: ran.append(1), initial_delay_seconds=0)
    scheduler.start()

    await wait_until_async(lambda: bool(ran), timeout=5.0)
    await scheduler.stop()

    assert ran == [1]  # 首轮跑了、周期没到（3600s）


async def test_initial_delay_defers_first_run():
    """``initial_delay_seconds`` 给定时首轮延后（只改写首轮触发时刻）。"""
    delay = 0.1
    scheduler = BackgroundScheduler()
    ran: list[int] = []
    scheduler.add_job("later", 0.02, lambda: ran.append(1), initial_delay_seconds=delay)
    scheduler.start()
    started = time.monotonic()

    await wait_until_async(lambda: bool(ran), timeout=5.0)
    elapsed = time.monotonic() - started
    await scheduler.stop()

    # 下界断言：参数被忽略（首轮退化成按 0.02 的 interval 排）或延迟被压小都会立刻红；
    # 负载只会让它更晚，不会假失败。
    assert elapsed >= delay, elapsed
    assert ran  # 窗口过后照常跑（并按周期续跑）
