"""打断取消阶梯测试（bounded cancel ladder）。

被守的语义（`libs/core/wing/agent/core.py::_stop_worker`）：

- 一次 `Task.cancel()` 可能被吸收（取消计数已增加、CancelledError 未投递
  ——asyncio 的已知形态）：阶梯重投取消（对挂起的 Future/Task 会重新投递，
  典型吸收形态可被击穿），worker 在有界时间内被杀掉、interrupt 正常重建；
- worker 对所有取消都不响应（极端形态）：interrupt 仍然**有界**返回，
  **保留旧 worker**（绝不重建第二个，避免两个 worker 抢同一个 inbox），
  打 ERROR 并广播 notice；被保留的 worker 随后若自然终止，终局续期自动
  重建消费者（否则消息进 inbox 无人消费）；
- 重投取消前重放 interrupt hooks（第一次取消被吞后 worker 可能又起了新
  子进程）；
- `shutdown()` 共用同一阶梯——worker 不响应时会话拆解（逐出 / release /
  模板切换）也不会被拖死，且不会被终局续期或 interrupt 重建复活（`_closing`
  闸门）；
- hooks 在拿到 `_interrupt_lock` 之后才执行：排队中的请求不提前杀 turn 的
  前台工具；积压输入在 interrupt 入口（等锁之前）清——锁等待期间新到的
  消息不被排队中的 interrupt 吞掉；
- `_worker_frames` 穿透常见挂起形态（`await <Task>` / `gather` / `async for`）
  ——阶梯耗尽时的现场日志要能看到真正的暂停点。

阶梯的时间参数在测试里被缩小（monkeypatch 模块常量），保持毫秒级。
"""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Iterator
from typing import Any

import pytest

from wing.agent import WingAgent
from wing.agent.core import _INTERRUPT_WAIT_SECONDS, _worker_frames
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


async def _suspended_frames(coro: Any) -> str:
    """跑起协程、等它挂起，返回 `_worker_frames` 文本（随后取消、收尸）。"""
    task = asyncio.create_task(coro)
    await asyncio.sleep(0.05)
    text = _worker_frames(task)
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)
    return text


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
    async def test_queued_interrupt_preserves_messages_arriving_during_wait(
        self, runtime, monkeypatch
    ) -> None:
        """积压在 interrupt 入口清；锁等待期间新到的消息不被吞掉。

        现场场景：打断没生效 → 发一条纠正消息 → 再按一次打断——第二条消息
        正好落在两次 interrupt 之间（排队中的那个在等锁）。旧实现把 clear
        推迟到拿锁之后，这条已 POST 成功（客户端收到 ok）的消息被静默吞掉。
        """
        _shrink_ladder(monkeypatch)
        agent = runtime.create_session().agent
        stop, started, swallow_all = _swallow_all_factory()
        stubborn = await _replace_worker(agent, swallow_all(), started=started)

        await agent._interrupt_lock.acquire()
        await agent.post("stale backlog before interrupt")
        queued = asyncio.create_task(agent.interrupt())
        await asyncio.sleep(0.05)  # 排队 interrupt 已执行入口清理、正等锁
        await agent.post("correction sent during lock wait")
        agent._interrupt_lock.release()
        await asyncio.wait_for(queued, timeout=5.0)

        # 入口时刻的积压放弃；等待期到达的消息幸存（留给重建后的消费者）。
        # 先收尾再断言：断言失败时不让吞取消的 worker 泄漏到 loop 关闭。
        survivors = [b.message.content for b in agent._inbox.drain()]
        await _dispose(stubborn, stop)
        await agent.shutdown()

        assert survivors == ["correction sent during lock wait"]

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

    @pytest.mark.asyncio
    async def test_interrupt_after_shutdown_never_rebuilds(self, runtime) -> None:
        """shutdown 是终局：其后的 interrupt 不重建 worker（不复活已关闭 agent）。

        可达路径：模板切换 / 逐出期间 shutdown 已收口，而在飞的 interrupt
        排在它的锁之后——重建条件若不看 `_closing`，会在已关闭的 agent
        上留下一个永不终止的消费者（任务泄漏 + provider 已关的残骸复活）。
        """
        agent = runtime.create_session().agent
        await agent.shutdown()
        worker = agent._worker
        assert worker.done()

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        assert agent._worker is worker
        assert worker.done()

    @pytest.mark.asyncio
    async def test_re_cancel_refires_hooks(self, runtime, monkeypatch) -> None:
        """重投取消前重放 hooks：第一次取消被吞后可能又起了新子进程。"""
        _shrink_ladder(monkeypatch)
        agent = runtime.create_session().agent

        fired: list[str] = []
        agent.register_interrupt_hook(lambda: fired.append("hook"))

        started = asyncio.Event()

        async def swallow_once() -> None:
            started.set()
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                await asyncio.sleep(30)  # 吞掉第一次投递，继续运行

        await _replace_worker(agent, swallow_once(), started=started)
        await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        # 入口一次 + 重投前一次（worker 被第二次投递杀死，无第三次重投）。
        assert fired == ["hook", "hook"]
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_undead_worker_late_death_triggers_renewal(
        self, runtime, monkeypatch
    ) -> None:
        """阶梯放手后 worker 迟死：终局续期自动重建消费者（消息不落空）。"""
        _shrink_ladder(monkeypatch, wait=0.02)
        agent = runtime.create_session().agent

        started = asyncio.Event()

        async def die_late() -> None:
            started.set()
            for _ in range(3):
                try:
                    await asyncio.sleep(30)
                except asyncio.CancelledError:
                    pass
            await asyncio.sleep(0.3)  # 阶梯之外才自然结束

        stubborn = await _replace_worker(agent, die_late(), started=started)

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)
        assert agent._worker is stubborn  # 先保留（绝不重建第二个）

        await _wait_until(lambda: agent._worker is not stubborn, timeout=3.0)
        assert stubborn.done()
        assert not agent._worker.done()
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupt_cancelled_while_worker_dying_renews(
        self, runtime, monkeypatch
    ) -> None:
        """interrupt 自身被取消、worker 随后死亡：终局续期同样兜住消费者。"""
        _shrink_ladder(monkeypatch, wait=1.0)
        agent = runtime.create_session().agent

        started = asyncio.Event()

        async def slow_death() -> None:
            started.set()
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                await asyncio.sleep(0.2)  # 收尸很慢

        worker = await _replace_worker(agent, slow_death(), started=started)

        task = asyncio.create_task(agent.interrupt())
        await asyncio.sleep(0.1)  # interrupt 已 cancel worker、正等在有界等待里
        task.cancel()  # 模拟上层取消在途请求（客户端断开 / 关停）
        await asyncio.wait_for(asyncio.gather(task, return_exceptions=True), 2.0)

        assert not worker.done()  # 尚未死：保留 + 续期已登记
        await _wait_until(lambda: agent._worker is not worker, timeout=3.0)
        assert not agent._worker.done()
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_queued_interrupts_both_return(self, runtime) -> None:
        """持锁者让路后，排队的两个 interrupt 都返回且各触发一次 hooks。"""
        agent = runtime.create_session().agent

        fired: list[str] = []
        agent.register_interrupt_hook(lambda: fired.append("hook"))

        await agent._interrupt_lock.acquire()
        first = asyncio.create_task(agent.interrupt())
        second = asyncio.create_task(agent.interrupt())
        await asyncio.sleep(0.05)
        assert fired == []  # 排队中：副作用不入场
        agent._interrupt_lock.release()
        await asyncio.wait_for(asyncio.gather(first, second), timeout=5.0)

        assert fired == ["hook", "hook"]
        assert not agent._worker.done()
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_retry_interrupt_kills_kept_worker(
        self, runtime, monkeypatch
    ) -> None:
        """undead 保留旧 worker 后，再次 interrupt 的第二投递把它杀死并重建。"""
        import wing.agent.core as core

        _shrink_ladder(monkeypatch)
        monkeypatch.setattr(core, "_INTERRUPT_MAX_CANCELS", 1)  # 第一次只投一次
        agent = runtime.create_session().agent

        started = asyncio.Event()

        async def swallow_once() -> None:
            started.set()
            try:
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                await asyncio.sleep(30)  # 吞掉第一次投递，继续运行

        stubborn = await _replace_worker(agent, swallow_once(), started=started)

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)
        assert agent._worker is stubborn and not stubborn.done()  # 保留（undead）

        monkeypatch.setattr(core, "_INTERRUPT_MAX_CANCELS", 3)
        await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        assert stubborn.done() and stubborn.cancelled()
        assert agent._worker is not stubborn
        assert not agent._worker.done()
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_shutdown_side_effects_wait_for_lock(self, runtime) -> None:
        """shutdown 同样只在拿到锁之后执行副作用。"""
        agent = runtime.create_session().agent

        fired: list[str] = []
        agent.register_interrupt_hook(lambda: fired.append("hook"))

        await agent._interrupt_lock.acquire()
        task = asyncio.create_task(agent.shutdown())
        await asyncio.sleep(0.05)
        assert fired == []
        agent._interrupt_lock.release()
        await asyncio.wait_for(task, timeout=5.0)
        assert fired == ["hook"]

    def test_interrupt_wait_covers_tool_gather_timeout(self) -> None:
        """单次阶梯等待覆盖工具收尸 5s 兜底 + 提交余量（常量联动）。

        重投取消若落在收尸路径内（清理 gather 挂住越过收尸兜底时会发生），
        `InterruptedToolResults` 不再抛出、本轮 partial 提交整体丢失——两个
        常量之一被调整时必须重审（见 core.py 阶梯注释）。
        """
        from wing.agent import tool_executor

        assert _INTERRUPT_WAIT_SECONDS >= tool_executor._INTERRUPT_GATHER_TIMEOUT + 1.0


class TestWorkerFrames:
    """`_worker_frames` 穿透常见挂起形态（阶梯耗尽现场日志的素材）。"""

    @pytest.mark.asyncio
    async def test_idle_worker_shows_the_await_chain(self, runtime) -> None:
        """空闲 worker（等 inbox）：链一路走到队列 get——真实形态的冒烟。"""
        agent = runtime.create_session().agent
        await asyncio.sleep(0.2)  # 让 worker 真正挂起在 inbox.get 上

        text = _worker_frames(agent._worker)

        assert ":run_turn" in text and ":_run" in text, text
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_follows_awaited_task_frames(self) -> None:
        """`await <Task>`：`cr_await` 是 FutureIter（无帧），经 referents 下钻。"""

        async def inner() -> None:
            await asyncio.sleep(30)

        async def outer() -> None:
            await asyncio.ensure_future(inner())

        text = await _suspended_frames(outer())
        assert ":outer" in text and ":inner" in text, text

    @pytest.mark.asyncio
    async def test_follows_async_generator_frames(self) -> None:
        """`async for`：经 asend 包装器（无帧）下钻到生成器帧。"""

        async def stream():
            yield 1
            await asyncio.sleep(30)
            yield 2

        async def consumer() -> None:
            async for _ in stream():
                pass

        text = await _suspended_frames(consumer())
        assert ":consumer" in text and ":stream" in text, text

    @pytest.mark.asyncio
    async def test_descends_single_child_gather(self) -> None:
        """单 child 的 gather：经 `_children` 下钻到 child 的协程帧。"""

        async def child() -> None:
            await asyncio.sleep(30)

        async def joiner() -> None:
            await asyncio.gather(child())

        text = await _suspended_frames(joiner())
        assert ":joiner" in text and ":child" in text, text

    @pytest.mark.asyncio
    async def test_marks_multi_child_gather_without_guessing(self) -> None:
        """多 child 的 gather：只标注 `<gather ×N>`，不猜哪条分支。"""

        async def child() -> None:
            await asyncio.sleep(30)

        async def joiner() -> None:
            await asyncio.gather(child(), child())

        text = await _suspended_frames(joiner())
        assert "<gather ×2>" in text, text
