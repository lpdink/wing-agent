# wing/agent/cancel_watch.py
"""Cancel 可观测性——worker 取消快照 / 不死看门狗 / interrupt 锁争用告警。

背景（2026-09-28 现场）：`interrupt()` 里 `Task.cancel()` 只调一次、`await old`
无超时，且全程持 `_interrupt_lock`。CPython 自己明言一次 cancel 可能不生效
（`Task.cancel()` 注释："we may have to cancel it again later"）——一旦如此，
`await old` 永久挂住、锁永不释放，后续 interrupt 请求全部排队（表现为"只对这个
session 不返回"），而会话本身毫发无损（CancelledError 从未进入 wing 代码）。
当时没有任何日志能回答"cancel 那一刻 worker 挂在哪、`_fut_waiter` 是什么"，
只能靠 lldb 注入进程取证。

本模块只做观测：读取 task 状态、写日志，不改变任何控制流；观测自身的异常
一律吞掉（取证手段不得成为新的故障源）。三件工具，按取证决定性排序：

- :func:`log_cancel_snapshot`：cancel 前打 worker 快照——**栈链 +
  `_fut_waiter` 类型**是判别器。类型三分支指向完全不同的吞没路径：
  `Future`（asyncio.Queue / httpx 读 / sleep）、`_GatheringFuture`（gather）、
  `Task`（在等另一个 task）。
- :func:`watch_undead_task`：cancel 后 T+1/5/15s 复查 worker 是否真死，
  未死则全量 dump（含完整栈链）。**换了暂停点继续跑** = cancel 已投递后被某帧
  吞掉；**纹丝不动** = 从未投递。
- :class:`InterruptLockWatch`：等锁 / 持锁超过阈值时周期 WARNING——
  把"死锁已经发生"从用户发现提前到日志发现。

栈链的覆盖范围（读日志须知）：沿协程等待链下钻 **协程 / async generator /
generator / Task / 单 child 的 gather**；`await Future`（httpx 读、队列 get、
sleep 等）在链末端只留下 `fut_waiter=` 字段（Future 本身没有帧）。链上出现
`<gather ×N>` 标记表示该处有 N 个并行 child、只标注不下钻。CPython 包装器
（`async for` 的 `async_generator_asend`、`FutureIter`）经 GC referents 兜底
下钻——`Task.get_stack()` 在 3.12 只给任务自身协程的顶层帧，只靠它看不到
真正的暂停点。
"""

from __future__ import annotations

import asyncio
import gc
import time
import types
from collections import deque
from collections.abc import AsyncIterator, Callable
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any

from wing.common.logger import log

#: cancel 后复查 worker 是否真死的时刻（相对 cancel，秒）。
UNDEAD_WATCH_SCHEDULE: tuple[float, ...] = (1.0, 5.0, 15.0)

#: 单行快照里取样的栈帧数（全量 dump 不受此限）。
STACK_DEPTH = 4

#: 沿等待链下钻的深度上限——保留**最内层**（真正的暂停点），防御异常形状的链。
FRAME_WALK_LIMIT = 64

#: 单字段 repr / repr 化的截断长度（一条日志不放论文）。
REPR_LIMIT = 200

#: 无帧的 CPython 包装器：唯一 awaitable 引用只在 GC referents 里（见 `_descend`）。
_REFERENT_WRAPPERS = frozenset(
    {"async_generator_asend", "async_generator_athrow", "FutureIter"}
)

#: GC referents 兜底只认这些类型，避免误抓无关引用。
_AWAITABLE_TYPES: tuple[type, ...] = (
    asyncio.Future,
    types.CoroutineType,
    types.AsyncGeneratorType,
    types.GeneratorType,
)


def _short(text: str, limit: int = REPR_LIMIT) -> str:
    return text if len(text) <= limit else f"{text[:limit]}…(+{len(text) - limit})"


def task_label(task: asyncio.Task[Any]) -> str:
    """任务短标签：名字 + 对象地址（与 lldb 取证里的地址可对照）。"""
    try:
        name = task.get_name()
    except Exception:
        name = "?"
    return f"{name}(0x{id(task):x})"


def _frame_location(frame: Any) -> str:
    try:
        return (
            f"{Path(frame.f_code.co_filename).name}:"
            f"{frame.f_lineno}:{frame.f_code.co_name}"
        )
    except Exception:
        return "?"


def _frame_of(obj: Any) -> Any | None:
    """协程 / async generator / generator 的当前帧；其余对象返回 None。"""
    return (
        getattr(obj, "cr_frame", None)
        or getattr(obj, "ag_frame", None)
        or getattr(obj, "gi_frame", None)
    )


def _await_attr(obj: Any) -> Any | None:
    """协程族对象正在等待的下一跳（`await` / `async for` 的目标）。"""
    return (
        getattr(obj, "cr_await", None)
        or getattr(obj, "ag_await", None)
        or getattr(obj, "gi_yieldfrom", None)
    )


def _referent_awaitable(obj: Any) -> Any | None:
    """无帧包装器的唯一 awaitable 引用（asend / FutureIter）。

    这两类对象不暴露 await 目标：`async_generator_asend` 的引用表实测为
    `[async_generator]`，`FutureIter` 为 `[FutureIter 类型, Future/Task]`。
    只在**恰好一个**候选时下钻；多候选（如 gather 的多个 child）不猜。
    """
    try:
        candidates = [
            ref for ref in gc.get_referents(obj) if isinstance(ref, _AWAITABLE_TYPES)
        ]
    except Exception:  # noqa: BLE001 - 观测不得外溢
        return None
    return candidates[0] if len(candidates) == 1 else None


def _descend(obj: Any) -> tuple[Any | None, str | None]:
    """下钻一层：返回 (下一跳, 标记)。标记用于无法下钻但必须标注的分叉点。"""
    nxt = _await_attr(obj)
    if nxt is not None:
        return nxt, None
    if isinstance(obj, asyncio.Task):
        try:
            return obj.get_coro(), None
        except Exception:  # noqa: BLE001
            return None, None
    children = getattr(obj, "_children", None)  # asyncio.gather 的 _GatheringFuture
    if children is not None:
        try:
            kids = list(children)
        except Exception:  # noqa: BLE001
            return None, None
        if len(kids) == 1:
            return kids[0], None
        return None, f"<gather ×{len(kids)}>"
    if type(obj).__name__ in _REFERENT_WRAPPERS:
        return _referent_awaitable(obj), None
    return None, None


def _await_chain(receiver: Any, limit: int) -> list[Any]:
    """沿等待链收集步骤（帧对象或标记字符串），最外层在前。

    `deque(maxlen=limit)` 保证超长链时保留**最内层**（真正的暂停点）——
    lldb 现场关心的是链末端，而不是任务自身协程的入口帧。
    """
    steps: deque[Any] = deque(maxlen=limit)
    seen: set[int] = set()
    current = receiver
    while current is not None and id(current) not in seen:
        seen.add(id(current))
        frame = _frame_of(current)
        if frame is not None:
            steps.append(frame)
        nxt, marker = _descend(current)
        if marker is not None:
            steps.append(marker)
        current = nxt
    return list(steps)


def stack_trace(task: asyncio.Task[Any], *, depth: int | None = STACK_DEPTH) -> str:
    """任务栈链（最内层在前）；无法取栈时给描述性占位，绝不抛。

    `depth=None` 取全量；已完成任务取不到帧（`get_stack` 抛 RuntimeError，
    `get_coro` 的帧已析构）。覆盖范围见模块文档——`await Future` 的末端
    由 `fut_waiter=` 字段承担。
    """
    try:
        steps = _await_chain(task.get_coro(), FRAME_WALK_LIMIT)
    except Exception:  # noqa: BLE001
        steps = []
    if not steps:
        # 正在执行的任务：cr_frame 取不到（协程在跑），get_stack 给实时调用栈。
        try:
            steps = list(task.get_stack())
        except Exception as e:
            return f"<unavailable: {type(e).__name__}>"
    if not steps:
        return "<no frames: task not started / already done>"
    if depth is not None:
        steps = steps[-depth:]
    return " <- ".join(
        step if isinstance(step, str) else _frame_location(step)
        for step in reversed(steps)
    )


def fut_waiter_field(task: asyncio.Task[Any]) -> str:
    """worker 当时的阻塞对象——类型是判别吞没路径的第一手证据。"""
    fut = getattr(task, "_fut_waiter", None)
    if fut is None:
        return "None"
    try:
        if fut.cancelled():
            state = "cancelled"
        elif fut.done():
            state = "done"
        else:
            state = "pending"
    except Exception:
        state = "?"
    return f"{type(fut).__name__}({state}) {_short(repr(fut))}"


def task_summary(task: asyncio.Task[Any]) -> str:
    """单行 worker 快照：身份 / 生命周期 / cancelling 簿记 / 阻塞对象。"""
    return (
        f"task={task_label(task)} done={task.done()} "
        f"cancelling={task.cancelling()} "
        f"must_cancel={getattr(task, '_must_cancel', '?')} "
        f"fut_waiter={fut_waiter_field(task)}"
    )


def log_cancel_snapshot(task: asyncio.Task[Any], *, context: str) -> None:
    """`cancel()` 之前的 worker 快照（INFO，单行）——本条同时充当 cancel issued 标记。

    Args:
        task: 即将被 cancel 的 worker。
        context: 关联标签（session + request id），其余日志同前缀。
    """
    try:
        log.info(
            f"cancel snapshot [{context}]: {task_summary(task)} "
            f"stack=[{stack_trace(task)}]"
        )
    except Exception:  # noqa: BLE001 - 观测不得外溢
        pass


def dump_task_state(
    task: asyncio.Task[Any],
    *,
    context: str,
    note: str,
    extra: Callable[[], str] | None = None,
) -> None:
    """未死 / 异常现场的 worker 全量 dump（多行 WARNING，每行同前缀可 grep）。"""
    lines = [f"{note}: {task_summary(task)}"]
    try:
        lines.append(f"stack=[{stack_trace(task, depth=None)}]")
    except Exception:  # noqa: BLE001
        lines.append("stack=<dump failed>")
    if extra is not None:
        try:
            lines.append(extra())
        except Exception:  # noqa: BLE001
            lines.append("extra=<unavailable>")
    for line in lines:
        try:
            log.warning(f"cancel watchdog [{context}]: {line}")
        except Exception:  # noqa: BLE001 - 观测不得外溢
            pass


async def watch_undead_task(
    task: asyncio.Task[Any],
    *,
    context: str,
    extra: Callable[[], str] | None = None,
    schedule: tuple[float, ...] = UNDEAD_WATCH_SCHEDULE,
) -> None:
    """cancel 后的看门狗——到点复查 worker 是否已死，未死则全量 dump。

    到点前一直等 worker 结束（`asyncio.wait` 不打断它、不消费它的异常）。
    worker 在第一个到点时刻前死亡（绝大多数情况）时整个看门狗零日志、
    零额外开销。最后一次复查后退出——不常驻、不重试 cancel。

    注意：看门狗与被观测的 worker 跑在同一个事件循环里——**连看门狗也完全
    没有输出**时，签名是"日志停在 interrupt start 之后"，指向循环被同步调用
    卡住一类的问题，而不是 cancel 丢失。
    """

    async def _wait_until(at: float, since: float) -> bool:
        done, _ = await asyncio.wait({task}, timeout=max(at - since, 0.0))
        return bool(done)

    previous = 0.0
    try:
        for at in schedule:
            if await _wait_until(at, previous):
                return
            previous = at
            dump_task_state(
                task,
                context=context,
                note=f"worker still alive at T+{at:g}s after cancel",
                extra=extra,
            )
    except asyncio.CancelledError:
        return
    except Exception:  # noqa: BLE001 - 观测不得外溢
        return


class InterruptLockWatch:
    """`_interrupt_lock` 的观测层：等待 / 持有超阈值时周期 WARNING。

    `hold()` 是 `async with lock` 的可观测替身——锁语义（FIFO 等待、释放时机、
    异常传播）与裸上下文管理器完全一致；观测不参与任何控制决策。
    告警间隔按 `warn_interval → warn_max_interval` 翻倍退避：死锁持续时间越
    长越安静（封顶 60s，每 waiter 每小时 ≤60 条），但不会静默。
    """

    def __init__(
        self,
        *,
        warn_after: float = 5.0,
        warn_interval: float = 5.0,
        warn_max_interval: float = 60.0,
    ) -> None:
        self._warn_after = warn_after
        self._warn_interval = warn_interval
        self._warn_max_interval = warn_max_interval
        self._holder: str | None = None
        self._held_since = 0.0
        self._waiters = 0
        # 周期告警协程的强引用（asyncio 只持弱引用——不接住会被 GC 掐死）。
        self._monitors: set[asyncio.Task[Any]] = set()

    @staticmethod
    def _backoff(interval: float, cap: float) -> float:
        """下一次告警的间隔（翻倍、封顶）。纯函数，单独锁定。"""
        return min(interval * 2, cap)

    @property
    def waiters(self) -> int:
        """正在等锁的 interrupt 协程数（本观测层自记，非锁的实现细节）。"""
        return self._waiters

    @property
    def holder(self) -> str | None:
        """当前持锁者标签（无持有者时为 None）。"""
        return self._holder

    def holder_desc(self) -> str:
        """持锁者描述（含持有时长），供等待方与告警引用。"""
        if self._holder is None:
            return "<none>"
        return f"{self._holder} (held {time.monotonic() - self._held_since:.1f}s)"

    @asynccontextmanager
    async def hold(self, lock: asyncio.Lock, tag: str) -> AsyncIterator[Any]:
        """获取 `lock` 并在持有期间观测（等价于 `async with lock` + 日志）。"""
        started = time.monotonic()
        monitor = None
        self._waiters += 1
        if lock.locked():
            log.warning(
                f"interrupt lock busy [{tag}]: queued behind {self.holder_desc()} "
                f"(waiters={self._waiters})"
            )
            monitor = self._spawn(self._warn_while_waiting(tag), f"lock-wait:{tag}")
        try:
            await lock.acquire()
        finally:
            self._waiters -= 1
            self._cancel(monitor)

        self._holder = tag
        self._held_since = time.monotonic()
        log.info(
            f"interrupt lock acquired [{tag}]: waited "
            f"{int((time.monotonic() - started) * 1000)}ms "
            f"(queued={self._waiters})"
        )
        hold_monitor = self._spawn(self._warn_while_holding(tag), f"lock-hold:{tag}")
        try:
            yield lock
        finally:
            self._cancel(hold_monitor)
            held_ms = int((time.monotonic() - self._held_since) * 1000)
            self._holder = None
            lock.release()
            log.info(f"interrupt lock released [{tag}]: held {held_ms}ms")

    # ── 内部 ──

    def _spawn(self, coro: Any, name: str) -> asyncio.Task[Any]:
        task = asyncio.create_task(coro, name=name)
        self._monitors.add(task)
        task.add_done_callback(self._monitors.discard)
        return task

    @staticmethod
    def _cancel(task: asyncio.Task[Any] | None) -> None:
        if task is not None:
            task.cancel()

    async def _warn_while_waiting(self, tag: str) -> None:
        """等锁超过 `warn_after` 后周期告警（典型现场：持锁者卡在 `await old`）。"""
        started = time.monotonic()
        interval = self._warn_interval
        try:
            await asyncio.sleep(self._warn_after)
            while True:
                log.warning(
                    f"interrupt lock still NOT acquired [{tag}]: waiting "
                    f"{time.monotonic() - started:.1f}s behind "
                    f"{self.holder_desc()} (waiters={self._waiters})"
                )
                interval = self._backoff(interval, self._warn_max_interval)
                await asyncio.sleep(interval)
        except asyncio.CancelledError:
            return
        except Exception:  # noqa: BLE001 - 观测不得外溢
            return

    async def _warn_while_holding(self, tag: str) -> None:
        """持锁超过 `warn_after` 后周期告警（典型现场：`await old` 永不返回）。"""
        interval = self._warn_interval
        try:
            await asyncio.sleep(self._warn_after)
            while True:
                log.warning(
                    f"interrupt lock held [{tag}] for "
                    f"{time.monotonic() - self._held_since:.1f}s — holder stuck? "
                    f"(waiters={self._waiters})"
                )
                interval = self._backoff(interval, self._warn_max_interval)
                await asyncio.sleep(interval)
        except asyncio.CancelledError:
            return
        except Exception:  # noqa: BLE001 - 观测不得外溢
            return


__all__ = [
    "FRAME_WALK_LIMIT",
    "REPR_LIMIT",
    "STACK_DEPTH",
    "UNDEAD_WATCH_SCHEDULE",
    "InterruptLockWatch",
    "dump_task_state",
    "fut_waiter_field",
    "log_cancel_snapshot",
    "stack_trace",
    "task_label",
    "task_summary",
    "watch_undead_task",
]
