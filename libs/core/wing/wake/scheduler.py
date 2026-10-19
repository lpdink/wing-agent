"""wing/wake/scheduler.py — 定时触发器的调度循环（单 asyncio task）。

形状与 ``wing/background.py`` 同族（单 task、顺序驱动、生命周期显式 start/stop），差别在
**没有固定周期**：睡到最近一条 ``next_fire_at_ms``，被 ``notify()`` 打断重算。

两条必须守住的细节：

  - ``asyncio.wait_for(timeout<=0)`` 走特例分支**立即超时**（连已 set 的停止事件都不观察），
    所以等待时长有下限 ``_MIN_TICK_SECONDS``——「到期立即触发」是把时长抬到下限，不是给 0；
  - 时钟可注入（epoch 毫秒）。单测推进假时钟 + ``notify()`` 就能覆盖到期 / 未到期 / 同一 tick
    多个触发器 / 单个失败不影响其它，全程零 sleep。

触发器状态的所有写入都在 :class:`~wing.wake.service.WakeService`（本类只做时间与顺序）。
"""

from __future__ import annotations

import asyncio
import time
from typing import TYPE_CHECKING, Callable, Protocol

from wing.common.logger import log

if TYPE_CHECKING:
    from wing.wake.model import WakeTrigger


class TriggerSource(Protocol):
    """调度器需要的服务面（避免 scheduler ↔ service 的循环 import）。"""

    async def recover_missed(self) -> int: ...

    def due_timers(self, now_ms: int) -> list[WakeTrigger]: ...

    async def fire_timer(self, trigger: WakeTrigger, now_ms: int) -> None: ...

    def next_due_ms(self) -> int | None: ...


def _default_clock_ms() -> int:
    return int(time.time() * 1000)


class WakeScheduler:
    """定时触发器调度循环（单 task，start/stop 幂等）。"""

    #: 等待时长下限（秒）。见模块 docstring：``wait_for(timeout<=0)`` 会立刻超时。
    _MIN_TICK_SECONDS = 0.01

    def __init__(
        self,
        source: TriggerSource,
        *,
        clock: Callable[[], int] = _default_clock_ms,
    ) -> None:
        self._source = source
        self._clock = clock
        self._task: asyncio.Task[None] | None = None
        self._wake = asyncio.Event()
        self._stopping = False

    @property
    def running(self) -> bool:
        return self._task is not None

    def start(self) -> None:
        """启动循环（幂等；需在运行中的事件循环内调用）。

        第一棒是重启恢复（``recover_missed``）：它要 await 投递，只能在有循环的 task 里跑。
        """
        if self._task is not None:
            return
        self._stopping = False
        self._wake.clear()
        self._task = asyncio.create_task(self._run())

    async def stop(self) -> None:
        """停止循环（幂等）：置停止位 + 打断等待，然后等 task 收尾。"""
        task, self._task = self._task, None
        if task is None:
            return
        self._stopping = True
        self._wake.set()
        try:
            await task
        except asyncio.CancelledError:
            if not task.cancelled():
                raise
        except Exception as exc:  # 循环自身异常也不能从 stop 里逃出去
            log.error(f"wake: scheduler loop died with error: {exc!r}")

    def notify(self) -> None:
        """打断当前等待、立刻重算（创建 / 取消 / 触发后）。"""

        self._wake.set()

    def next_timeout_seconds(self) -> float | None:
        """到下一个到期时刻还要等多久（``None`` = 没有定时触发器）。"""
        next_due = self._source.next_due_ms()
        if next_due is None:
            return None
        return max(self._MIN_TICK_SECONDS, (next_due - self._clock()) / 1000)

    async def fire_due(self) -> int:
        """触发所有已到期的 timer（按排期顺序），返回实际处理的条数。

        单条失败只记日志，不影响其它条（与 ``BackgroundScheduler`` 的 job 异常隔离同款）。
        """
        triggers = self._source.due_timers(self._clock())
        for trigger in triggers:
            try:
                await self._source.fire_timer(trigger, self._clock())
            except Exception:
                log.exception(f"wake: trigger {trigger.id} failed")
        return len(triggers)

    # ── 内部 ─────────────────────────────────────

    async def _run(self) -> None:
        try:
            await self._source.recover_missed()
        except Exception:
            log.exception("wake: startup recovery failed")
        while not self._stopping:
            self._wake.clear()
            timeout = self.next_timeout_seconds()
            try:
                await asyncio.wait_for(self._wake.wait(), timeout=timeout)
                continue  # 被打断：重新计算（创建 / 取消 / 触发都会走这里）
            except TimeoutError:
                pass
            if self._stopping:
                return
            await self.fire_due()
