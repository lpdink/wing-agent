"""BackgroundScheduler：幂等、无人认领时的等待、循环异常与取消语义。

调度器是周期任务的宿主（首个 job 是空闲会话逐出），它的 stop 语义直接决定
gateway 关闭时会不会挂住或假装正常返回。
"""

from __future__ import annotations

import asyncio

import pytest

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
    await asyncio.sleep(0.05)

    await scheduler.stop()

    assert ran == []


async def test_due_job_runs_and_reschedules():
    scheduler = BackgroundScheduler()
    ran: list[int] = []
    scheduler.add_job("tick", 0.02, lambda: ran.append(1))
    scheduler.start()

    await asyncio.sleep(0.12)
    await scheduler.stop()

    assert len(ran) >= 2  # 多轮触发：跑完先排下一次，不忙循环
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

    await asyncio.sleep(0.1)
    await scheduler.stop()

    assert healthy  # 健康 job 照常跑
    assert any("job 'boom' failed" in m for m in wing_warnings)


async def test_stop_waits_for_running_job():
    """stop 等正在执行的 job 跑完（不再触发后续轮次）。"""
    scheduler = BackgroundScheduler()
    finished: list[str] = []

    async def slow() -> None:
        await asyncio.sleep(0.05)
        finished.append("done")

    scheduler.add_job("slow", 0.01, slow)
    scheduler.start()
    await asyncio.sleep(0.03)  # 让 slow 开始执行

    await scheduler.stop()

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

    scheduler.remove_job("a")
    scheduler.remove_job("missing")  # 未知名称静默忽略
    assert scheduler.jobs == []
