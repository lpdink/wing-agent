"""interrupt 可观测性（cancel 快照 / 不死看门狗 / 锁争用告警）的对账测试。

背景与判读方式见 ``wing/agent/cancel_watch.py`` 模块文档：一次未生效的
``Task.cancel()`` 会让 ``await old`` 永久挂住、``_interrupt_lock`` 永不释放
（2026-09-28 事故现场，只能靠 lldb 注入取证）。这些测试锁定"下次能靠日志
破案"所需的证据：

  - cancel 前的 worker 快照（含 ``_fut_waiter`` 类型与栈顶——吞没路径判别器）；
  - cancel 后 worker 未死的看门狗 dump（T+1/5/15s 复查）；
  - 锁等待 / 持有超阈值的周期 WARNING；
  - interrupt 的分段日志与 hook label（pid 归因）；
  - worker 循环边界的 cancelling 留痕（投递未死的下一次迭代立刻可见）；
  - asyncio 未处理异常的兜底日志（L8）。
"""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Iterator
from typing import Any

import pytest

from wing.agent.cancel_watch import (
    InterruptLockWatch,
    log_cancel_snapshot,
    watch_undead_task,
)
from wing.common.logger import install_loop_exception_logger


@pytest.fixture()
def wing_logs(caplog: pytest.LogCaptureFixture) -> Iterator[pytest.LogCaptureFixture]:
    """捕获 wing logger 的记录。

    库 logger 的 ``propagate=False``——记录不会进 root logger，caplog 必须把
    自己的 handler 挂到 wing logger 上才看得到。
    """
    caplog.set_level(logging.DEBUG, logger="wing")
    logger = logging.getLogger("wing")
    logger.addHandler(caplog.handler)
    yield caplog
    logger.removeHandler(caplog.handler)


@pytest.fixture()
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


async def _enter_lock(watch: InterruptLockWatch, lock: asyncio.Lock, tag: str) -> None:
    """在子任务里拿锁（供"等待中被打断"测试用）。"""
    async with watch.hold(lock, tag):
        return


class TestCancelSnapshot:
    """L1：cancel 前的 worker 快照。"""

    @pytest.mark.asyncio
    async def test_reports_fut_waiter_type_and_stack(self, wing_logs) -> None:
        """挂在队列上的 worker：fut_waiter 是 Future、栈顶可见 await 点。"""
        queue: asyncio.Queue[int] = asyncio.Queue()

        async def worker() -> None:
            await queue.get()

        task = asyncio.create_task(worker(), name="Task-probe")
        await asyncio.sleep(0)
        log_cancel_snapshot(task, context="sess-1 req=deadbeef")
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

        text = wing_logs.text
        assert "cancel snapshot [sess-1 req=deadbeef]" in text
        assert "task=Task-probe(" in text, text
        assert "done=False" in text
        assert "cancelling=0" in text
        assert "fut_waiter=Future(pending)" in text, text
        # 栈顶（最内层在前）：沿 await 链下钻到真正的暂停点（get_stack 只给顶层帧）。
        assert "queues.py:" in text and ":get <- " in text, text
        assert ":worker" in text, text

    @pytest.mark.asyncio
    async def test_done_task_snapshot_never_raises(self, wing_logs) -> None:
        """已完成任务的取栈会抛 RuntimeError——快照必须降级为占位而不是外溢。"""

        async def worker() -> None:
            return

        task = asyncio.create_task(worker())
        await task
        log_cancel_snapshot(task, context="sess-done")

        text = wing_logs.text
        assert "cancel snapshot [sess-done]" in text
        assert "done=True" in text
        assert "stack=[<no frames" in text or "stack=[<unavailable" in text


class TestUndeadWatchdog:
    """L2：cancel 后 worker 未死的看门狗。"""

    @pytest.mark.asyncio
    async def test_dumps_worker_that_swallowed_cancel(self, wing_logs) -> None:
        """吞掉 CancelledError 继续跑的 worker：到点复查未死 → 全量 dump。"""
        swallowed = asyncio.Event()

        async def stubborn() -> None:
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                swallowed.set()
                await asyncio.sleep(30)  # 模拟"cancel 被吞"：协程继续活着

        task = asyncio.create_task(stubborn(), name="Task-stubborn")
        await asyncio.sleep(0)
        task.cancel()
        await asyncio.wait_for(swallowed.wait(), timeout=1.0)

        await watch_undead_task(
            task,
            context="sess-2 req=cafe",
            extra=lambda: "agent: working=True",
            schedule=(0.02, 0.05),
        )
        assert not task.done()

        text = wing_logs.text
        assert "cancel watchdog [sess-2 req=cafe]" in text
        assert "worker still alive at T+0.02s after cancel" in text
        assert "cancelling=1" in text
        assert "stack=[" in text
        assert "agent: working=True" in text
        # 复查按计划表推进（两次都未死 → 两次 dump）
        assert "worker still alive at T+0.05s after cancel" in text

        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    @pytest.mark.asyncio
    async def test_silent_when_worker_dies_promptly(self, wing_logs) -> None:
        """正常 worker：cancel 后立刻死亡 → 看门狗零日志、零等待。"""

        async def cooperative() -> None:
            await asyncio.sleep(30)

        task = asyncio.create_task(cooperative())
        await asyncio.sleep(0)
        task.cancel()
        started = asyncio.get_running_loop().time()
        await watch_undead_task(task, context="sess-3", schedule=(5.0,))
        elapsed = asyncio.get_running_loop().time() - started

        assert task.done()
        assert elapsed < 1.0
        assert "cancel watchdog" not in wing_logs.text


class TestInterruptLockWatch:
    """L3：interrupt 锁的等待 / 持有告警（锁语义保持不变）。"""

    @pytest.mark.asyncio
    async def test_acquire_and_release_logged(self, wing_logs) -> None:
        watch = InterruptLockWatch(warn_after=5.0, warn_interval=5.0)
        lock = asyncio.Lock()

        async with watch.hold(lock, "sess-4 req=01"):
            assert lock.locked()
            assert watch.holder == "sess-4 req=01"

        assert not lock.locked()
        assert watch.holder is None
        text = wing_logs.text
        assert "interrupt lock acquired [sess-4 req=01]" in text
        assert "interrupt lock released [sess-4 req=01]" in text

    @pytest.mark.asyncio
    async def test_warns_when_held_too_long(self, wing_logs) -> None:
        watch = InterruptLockWatch(warn_after=0.02, warn_interval=0.02)
        lock = asyncio.Lock()

        async with watch.hold(lock, "sess-5"):
            await asyncio.sleep(0.08)

        assert "interrupt lock held [sess-5] for" in wing_logs.text
        assert "holder stuck?" in wing_logs.text

    @pytest.mark.asyncio
    async def test_warns_when_waiting_too_long(self, wing_logs) -> None:
        watch = InterruptLockWatch(warn_after=0.02, warn_interval=0.02)
        lock = asyncio.Lock()

        async def holder() -> None:
            async with watch.hold(lock, "holder"):
                await asyncio.sleep(0.15)

        holding = asyncio.create_task(holder())
        await asyncio.sleep(0.02)
        async with watch.hold(lock, "waiter"):
            pass
        await holding

        text = wing_logs.text
        assert "interrupt lock busy [waiter]" in text
        assert "interrupt lock still NOT acquired [waiter]" in text
        assert "queued behind holder" in text

    @pytest.mark.asyncio
    async def test_releases_lock_on_body_error(self) -> None:
        watch = InterruptLockWatch(warn_after=5.0, warn_interval=5.0)
        lock = asyncio.Lock()

        with pytest.raises(RuntimeError, match="boom"):
            async with watch.hold(lock, "sess-6"):
                raise RuntimeError("boom")

        assert not lock.locked()
        assert watch.waiters == 0
        assert watch.holder is None

    @pytest.mark.asyncio
    async def test_cancelled_waiter_leaves_lock_usable(self) -> None:
        watch = InterruptLockWatch(warn_after=5.0, warn_interval=5.0)
        lock = asyncio.Lock()

        async def holder() -> None:
            async with watch.hold(lock, "holder"):
                await asyncio.sleep(0.1)

        holding = asyncio.create_task(holder())
        await asyncio.sleep(0.02)
        waiter = asyncio.create_task(_enter_lock(watch, lock, "waiter"))
        await asyncio.sleep(0.02)
        assert watch.waiters == 1

        waiter.cancel()
        with pytest.raises(asyncio.CancelledError):
            await waiter
        assert watch.waiters == 0

        await holding
        assert not lock.locked()
        assert watch.holder is None


class TestAgentInterruptLogs:
    """agent 侧分段日志：start → hooks → lock → cancel → retired → reset。"""

    @pytest.mark.asyncio
    async def test_interrupt_logs_stages_and_hook_labels(
        self, runtime, wing_logs
    ) -> None:
        session = runtime.create_session()
        agent = session.agent
        fired: list[str] = []
        agent.register_interrupt_hook(
            lambda: fired.append("hook"), label="Bash pid=4242"
        )
        # 让 worker 跑到 inbox.get 挂起点（真实网关里 interrupt 到达时必然如此）。
        await asyncio.sleep(0)

        await agent.interrupt(request_id="deadbeefcafe")
        assert fired == ["hook"]  # 只在 interrupt 时触发（shutdown 前先断言）

        text = wing_logs.text
        assert "interrupt start [" in text
        assert "req=deadbeef" in text  # tag 截断到前 8 位
        assert "interrupt hooks [" in text and "Bash pid=4242" in text
        assert "cancel snapshot [" in text
        # 空闲 worker 挂在 inbox 队列上：fut_waiter 为 Future、栈链到 queues.get。
        assert "fut_waiter=Future(pending)" in text
        assert "queues.py:" in text and "inbox.py:" in text
        assert ":run_turn <- " in text and ":_run" in text
        assert "interrupt lock acquired [" in text
        assert "interrupt old worker retired [" in text
        assert "outcome=cancelled" in text
        assert "Agent interrupted and reset [" in text

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_worker_loop_boundary_traces_cancelling(
        self, runtime, wing_logs
    ) -> None:
        """L7：cancel 记账（cancelling 0→1）在循环边界以 INFO 留痕。"""
        session = runtime.create_session()
        agent = session.agent
        await asyncio.sleep(0)  # 空闲 worker 走到第一个循环边界

        agent._worker.cancel()
        agent._trace_cancelling()  # 不 yield：cancelling 已计入、投递尚未发生
        await asyncio.sleep(0.05)

        text = wing_logs.text
        assert "worker loop boundary" in text
        assert "cancelling=0" in text
        assert "cancelling 0 -> 1" in text

        await agent.shutdown()


class TestLoopExceptionLogger:
    """L8：asyncio 未处理异常的兜底日志（转发给原处理器）。"""

    @pytest.mark.asyncio
    async def test_logs_and_chains_to_previous_handler(self, wing_logs) -> None:
        loop = asyncio.get_running_loop()
        chained: list[dict[str, Any]] = []
        original = loop.get_exception_handler()
        loop.set_exception_handler(lambda _loop, context: chained.append(context))
        try:
            install_loop_exception_logger()
            handler = loop.get_exception_handler()
            assert handler is not None
            error = RuntimeError("probe boom")
            handler(
                loop,
                {
                    "message": "Task exception was never retrieved",
                    "exception": error,
                    "task": asyncio.current_task(),
                },
            )
            # 幂等：重复安装不叠处理器（依旧是同一个 handler）
            install_loop_exception_logger()
            assert loop.get_exception_handler() is handler
            handler(loop, {"message": "second"})
        finally:
            loop.set_exception_handler(original)

        messages = [record.message for record in wing_logs.records]
        assert sum("asyncio unhandled" in message for message in messages) == 2
        assert any("Task exception was never retrieved" in m for m in messages)
        assert "probe boom" in wing_logs.text
        # 原处理器（stderr 行为）被原样链上。
        assert [context["message"] for context in chained] == [
            "Task exception was never retrieved",
            "second",
        ]

    def test_no_running_loop_is_a_noop(self) -> None:
        """无运行中的事件循环时静默跳过（库导入零副作用）。"""
        install_loop_exception_logger()  # 不抛即通过
