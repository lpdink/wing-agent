# wing/background.py
"""BackgroundScheduler — 进程内周期任务宿主。

单 asyncio task 顺序驱动多个周期 job。**不是真线程**：job 本身是 async 的
（如逐出会话要 await provider client 关闭），线程只会把并发推到别处。

设计约束：
  - **job 异常隔离**：单个 job 抛错只记日志，不影响其它 job 与循环本身；
  - **无重入**：单 task 顺序执行，job 之间不重叠（也就没有并发问题）；
  - **生命周期显式**：start/stop 由进程宿主调用（gateway lifespan），
    不 start 就完全不跑——单测与嵌入式使用零副作用。

第一个 job 是 session 逐出（``SessionReaper``）；将来的后台机制
（dreaming 等）直接 ``add_job`` 复用同一宿主。
"""

from __future__ import annotations

import asyncio
import inspect
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any

from wing.common.logger import log

JobCallback = Callable[[], Awaitable[Any] | None]
"""周期 job 的形态：同步函数或协程函数（返回值忽略）。"""


@dataclass
class _Job:
    name: str
    interval: float
    callback: JobCallback
    next_run: float


class BackgroundScheduler:
    """周期任务宿主（单 task 顺序驱动）。

    用法::

        scheduler = BackgroundScheduler()
        scheduler.add_job("session-eviction", 300.0, reaper.sweep)
        scheduler.start()
        ...
        await scheduler.stop()
    """

    #: 最小等待间隔（秒）。job 时长超过 interval 时，`min(deadlines) - clock()`
    #: 会 ≤ 0，而 `asyncio.wait_for(timeout<=0)` 走特例分支**立即超时**——
    #: 连已 set 的停止事件也不观察（循环永远进不了 `except` 之外的分支）。
    #: 加一个下限，保证每轮都真的等待、都能看到停止信号。
    _MIN_TICK_SECONDS = 0.01

    def __init__(self, *, clock: Callable[[], float] = time.monotonic) -> None:
        self._clock = clock
        self._jobs: dict[str, _Job] = {}
        self._task: asyncio.Task[None] | None = None
        self._stop = asyncio.Event()

    @property
    def jobs(self) -> list[str]:
        """已注册的 job 名称（快照）。"""
        return list(self._jobs)

    @property
    def running(self) -> bool:
        return self._task is not None

    def add_job(
        self, name: str, interval_seconds: float, callback: JobCallback
    ) -> None:
        """注册周期 job（首次触发在 ``interval_seconds`` 之后）。

        Raises:
            ValueError: 间隔非正或名称重复
        """
        interval = float(interval_seconds)
        if interval <= 0:
            raise ValueError(f"job '{name}': interval must be > 0, got {interval}")
        if name in self._jobs:
            raise ValueError(f"job '{name}' is already registered")
        self._jobs[name] = _Job(
            name=name,
            interval=interval,
            callback=callback,
            next_run=self._clock() + interval,
        )
        log.info(f"Background job registered: {name} (every {interval:.1f}s)")

    def remove_job(self, name: str) -> None:
        """注销 job（未知名称静默忽略）。"""
        self._jobs.pop(name, None)

    def start(self) -> None:
        """启动调度循环（幂等；需在运行中的事件循环内调用）。"""
        if self._task is not None:
            return
        self._stop.clear()
        self._task = asyncio.create_task(self._run())
        log.info(f"BackgroundScheduler started: jobs={list(self._jobs)}")

    async def stop(self) -> None:
        """停止调度循环（幂等）。

        会等待**正在执行**的 job 跑完（`await task` 语义），但不会再触发
        任何后续轮次——调用方（gateway lifespan shutdown）需要的是
        "停止后不再有新的 job 启动"。
        """
        task, self._task = self._task, None
        if task is None:
            return
        self._stop.set()
        try:
            await task
        except asyncio.CancelledError:
            # 区分两种取消：调度循环自身被取消（如事件循环收尾）→ 吞掉；
            # 调用方自己被取消（await 被打断，循环其实没停）→ 继续向上传播
            # （吞掉会让 stop() 在外层取消/超时时假装"正常返回"）。
            if not task.cancelled():
                raise
        except Exception as e:  # 循环自身异常也不能从 stop 里逃出去
            log.error(f"BackgroundScheduler loop died with error: {e}")
        log.info("BackgroundScheduler stopped")

    # ── 内部 ──────────────────────────────────

    async def _run(self) -> None:
        while True:
            if self._stop.is_set():
                return
            deadlines = [job.next_run for job in self._jobs.values()]
            if not deadlines:
                await self._stop.wait()
                return
            wait = max(self._MIN_TICK_SECONDS, min(deadlines) - self._clock())
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=wait)
                return  # 收到停止信号
            except TimeoutError:
                pass
            await self._run_due_jobs()

    async def _run_due_jobs(self) -> None:
        now = self._clock()
        for job in list(self._jobs.values()):
            if job.next_run > now:
                continue
            # 先排下一次（即使本次失败也不进入忙循环）
            job.next_run = now + job.interval
            try:
                result = job.callback()
                if inspect.isawaitable(result):
                    await result
            except Exception:
                log.exception(f"Background job '{job.name}' failed")
