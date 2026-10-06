"""打断取消阶梯测试（bounded cancel ladder）。

被守的语义（`libs/core/wing/agent/core.py::_stop_worker`）：

- 一次 `Task.cancel()` 可能被吸收（取消计数已增加、CancelledError 未投递
  ——asyncio 的已知形态）：阶梯重投取消（对 >1 的计数强制重新投递），
  worker 在有界时间内被杀掉、interrupt 正常重建；
- worker 对所有取消都不响应（极端形态）：interrupt 仍然**有界**返回，
  **保留旧 worker**（绝不重建第二个，避免两个 worker 抢同一个 inbox），
  打 ERROR 并广播 notice；
- `shutdown()` 共用同一阶梯——worker 不响应时会话拆解（逐出 / release /
  模板切换）也不会被拖死；
- hook / inbox 清理等副作用在拿到 `_interrupt_lock` 之后才执行：排队中的
  请求不再提前杀 turn 的前台工具。

阶梯的时间参数在测试里被缩小（monkeypatch 模块常量），保持毫秒级。
"""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Iterator
from typing import Any

import pytest

from wing.agent import WingAgent
from wing.event import NoticeEvent
from wing.event_bus import event_bus


@pytest.fixture(autouse=True)
def cleanup_event_bus() -> Iterator[None]:
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture()
def wing_logs(caplog: pytest.LogCaptureFixture) -> Iterator[pytest.LogCaptureFixture]:
    """捕获 wing logger 的记录（库 logger propagate=False，需挂 handler）。"""
    caplog.set_level(logging.DEBUG, logger="wing")
    logger = logging.getLogger("wing")
    logger.addHandler(caplog.handler)
    yield caplog
    logger.removeHandler(caplog.handler)


@pytest.fixture()
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


def _shrink_ladder(monkeypatch: pytest.MonkeyPatch, *, wait: float = 0.05) -> None:
    """缩小单次阶梯等待，让测试保持毫秒级。"""
    import wing.agent.core as core

    monkeypatch.setattr(core, "_INTERRUPT_WAIT_SECONDS", wait)


async def _wait_until(pred: Any, timeout: float = 5.0) -> None:
    async def _inner() -> None:
        while not pred():
            await asyncio.sleep(0.005)

    await asyncio.wait_for(_inner(), timeout)


async def _replace_worker(
    agent: WingAgent, coro: Any, *, started: asyncio.Event | None = None
) -> asyncio.Task:
    """掐掉真实 worker，换成给定的任务（复刻现场：worker 吞取消）。

    现场形态是「挂起中的 worker 被 cancel」——对**未启动**的协程 cancel，
    CancelledError 在协程体执行前抛出、body 无从吞掉，因此传 `started` 时
    等任务真正开跑再返回。
    """
    original = agent._worker
    original.cancel()
    await asyncio.gather(original, return_exceptions=True)
    task = asyncio.create_task(coro)
    agent._worker = task
    if started is not None:
        await asyncio.wait_for(started.wait(), timeout=1.0)
    return task


def _swallow_all_factory() -> tuple[asyncio.Event, asyncio.Event, Any]:
    """永不响应取消的 worker 协程（吞掉每一次投递，直到 stop 置位）。"""
    stop = asyncio.Event()
    started = asyncio.Event()

    async def swallow_all() -> None:
        started.set()
        while not stop.is_set():
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                continue

    return stop, started, swallow_all


async def _dispose(stubborn: asyncio.Task, stop: asyncio.Event) -> None:
    stop.set()
    stubborn.cancel()
    await asyncio.wait_for(
        asyncio.gather(stubborn, return_exceptions=True), timeout=2.0
    )


class TestCancelLadder:
    """取消阶梯的收口语义。"""

    @pytest.mark.asyncio
    async def test_swallowed_cancel_is_re_delivered(self, runtime, monkeypatch) -> None:
        """吞掉第一次取消：重投取消将其杀死，interrupt 有界返回并重建。"""
        _shrink_ladder(monkeypatch)
        agent = runtime.create_session().agent

        started = asyncio.Event()
        swallowed = asyncio.Event()

        async def swallow_once() -> None:
            started.set()
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                swallowed.set()
                await asyncio.sleep(30)  # 吞掉第一次投递，继续运行

        stubborn = await _replace_worker(agent, swallow_once(), started=started)

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        # 第一次取消确实被吞（再投递才是杀死它的那一次）。
        assert swallowed.is_set()
        assert stubborn.done() and stubborn.cancelled()
        assert agent._worker is not stubborn
        assert not agent._worker.done()
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_unstoppable_worker_is_kept_not_rebuilt(
        self, runtime, monkeypatch, wing_logs
    ) -> None:
        """阶梯耗尽（永不响应）：保留旧 worker、不重建，ERROR + notice。"""
        _shrink_ladder(monkeypatch)
        agent = runtime.create_session().agent

        stop, started, swallow_all = _swallow_all_factory()
        stubborn = await _replace_worker(agent, swallow_all(), started=started)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        # 保留旧 worker——绝不重建第二个（两个 worker 会抢同一个 inbox）。
        assert agent._worker is stubborn
        assert not stubborn.done()
        # ERROR 现场 + 面向用户的 notice（前端提示"打断未生效"）。
        assert any("仍未终止" in r.getMessage() for r in wing_logs.records)
        assert any(isinstance(e, NoticeEvent) and e.level == "error" for e in events)

        await _dispose(stubborn, stop)
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_dead_worker_error_is_reported_and_replaced(
        self, runtime, wing_logs
    ) -> None:
        """worker 已带异常死亡：interrupt 报告该异常并正常重建。"""
        agent = runtime.create_session().agent

        async def boom() -> None:
            raise RuntimeError("worker crashed")

        dead = await _replace_worker(agent, boom())
        await _wait_until(dead.done)

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        assert dead.done() and not dead.cancelled()
        assert isinstance(dead.exception(), RuntimeError)
        assert agent._worker is not dead
        assert any(
            "died with error during interrupt" in r.getMessage()
            for r in wing_logs.records
        )
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_hook_side_effects_wait_for_lock(self, runtime) -> None:
        """等锁期间不执行副作用（hooks）；拿到锁之后才执行。"""
        agent = runtime.create_session().agent

        fired: list[str] = []
        agent.register_interrupt_hook(lambda: fired.append("hook"))

        await agent._interrupt_lock.acquire()
        task = asyncio.create_task(agent.interrupt())
        await asyncio.sleep(0.05)
        assert fired == []  # 排队中：不提前杀 turn 的前台工具
        agent._interrupt_lock.release()
        await asyncio.wait_for(task, timeout=5.0)
        assert fired == ["hook"]
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_shutdown_is_bounded_with_unstoppable_worker(
        self, runtime, monkeypatch
    ) -> None:
        """shutdown 共用阶梯：不响应的 worker 也拖不死会话拆解。"""
        _shrink_ladder(monkeypatch)
        agent = runtime.create_session().agent

        stop, started, swallow_all = _swallow_all_factory()
        stubborn = await _replace_worker(agent, swallow_all(), started=started)

        await asyncio.wait_for(agent.shutdown(), timeout=5.0)

        assert agent._worker is stubborn and not stubborn.done()
        await _dispose(stubborn, stop)
