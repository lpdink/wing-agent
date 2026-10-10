"""worker 韧性（#187）：持久化故障下的回合收口 / 消费者续期。

被守的语义（`libs/core/wing/agent/core.py` + `agent/event_sink.py`）：

- **报告路径不可失败**：兜底 handler 的收尾动作（turn_result / error / done）
  与它报告的故障常常同源——落盘正是刚坏掉的资源（如磁盘打满 ENOSPC）。严格
  发射会在 except 块里再抛一次，同层接不住：异常逃出 run_turn、再逃出 worker
  的 while True，消费者协程就此结束——status 仍是 idle、投递无任何反应
  （僵尸态），只能靠 interrupt 重建；
- **当前回合干净终止**：以 error 收口（working 复位、状态可被下一次投递干净
  使用；不卡在中途、不盲目重试同一个坏循环）；
- **消费者不死**：非意图性异常结束 → 自动重建；取消（interrupt / shutdown）
  是意图性收口，通用续期不抢建第二个消费者，`_closing` 闸门绝不复活已关闭的
  agent；
- **零僵尸态**：故障期间与恢复之后投递的消息都必须被消费（磁盘恢复后会话
  自然继续工作）。

注入方式与现场同形：让会话落盘（`history.jsonl` 写口）抛 OSError ENOSPC——
① 只让**报告动作**的落盘失败（issue 实录：回合提交成功、上报被击穿；同一
事故里另一些会话因盘上恰好有空隙而侥幸存活）；② 让整条持久化路径失败（磁盘
全满）。
"""

from __future__ import annotations

import asyncio
import contextlib
import errno
from collections.abc import Callable, Iterator
from typing import Any
from unittest.mock import patch

import pytest

from wing.event_bus import event_bus
from wing.agent.react_loop import ReActLoop
from wing.schema import LLMResponse, LLMUsage, TextBlock

#: 磁盘打满（现场故障：write 返回 ENOSPC）。
ENOSPC = OSError(errno.ENOSPC, "No space left on device")

#: 收尾三连（turn_result / error / done）——错误路径的「报告动作」。
REPORT_TYPES = {"turn_result", "error", "done"}


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    """每个测试前后清理全局 EventBus。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def runtime():
    """创建 WingRuntime 实例。"""
    from wing.runtime import WingRuntime

    return WingRuntime()


class WorkerEscape(BaseException):
    """非 Exception 的逃逸（故意穿透 `_run` 的两层兜底）。

    加固之后 Exception 已经出不了 worker 循环（报告窗口 + `_run` 的 while）；
    最后一道网（`_on_worker_done` 的通用续期）只能用 BaseException 形态验证
    ——它对应"我们没预料到的死法"。
    """


class _BoomStream:
    """模型流：迭代它时第一帧就是异常（`async for` 直接收到）。"""

    def __aiter__(self) -> "_BoomStream":
        return self

    async def __anext__(self) -> Any:
        raise WorkerEscape("escaped the drain loop")


async def _mock_generate(*args: Any, **kwargs: Any):
    """Mock LLM generate：返回一条纯文本响应（无 tool calls），结束 turn。"""
    yield LLMResponse(
        content="ok",
        content_blocks=[TextBlock(text="ok")],
        usage=LLMUsage(prompt_tokens=10, completion_tokens=5),
    )


def _boom_generate(*args: Any, **kwargs: Any) -> _BoomStream:
    """以 BaseException 逃出模型调用（穿透 with_retry 与两层兜底）。"""
    return _BoomStream()


def _persist_log(agent: Any) -> Any:
    """会话的落盘写口（`MessageLog`：文件后端即 history.jsonl）。

    锁定的内部契约：`ContextManager._messages`（TrackedList）→ `._log`
    （MessageLog）——落盘故障注入的唯一入口（生产故障就在这一层）。TrackedList
    换后端 / 改布局时这里会碎，属于预期（那时注入点要跟着挪）。
    """
    return agent.context_manager._messages._log


def _record_spawns(agent: Any, monkeypatch: pytest.MonkeyPatch) -> list[Any]:
    """记录该 agent 的消费者任务（以当前 `agent._worker` 起始），含孤儿。

    只看 `agent._worker` 抓不到"第二个消费者"——孤儿任务照样在抢同一个
    inbox（两边都 `_set_working(True)`、都往同一条链提交）。
    """
    spawned: list[Any] = [agent._worker]
    original = agent._spawn_worker

    def _record() -> Any:
        task = original()
        spawned.append(task)
        return task

    monkeypatch.setattr(agent, "_spawn_worker", _record)
    return spawned


@contextlib.contextmanager
def _capture_loop_callback_errors() -> Iterator[list[dict[str, Any]]]:
    """捕获 event loop 的异常上下文（done callback 里漏出的异常经此上报）。"""
    loop = asyncio.get_running_loop()
    contexts: list[dict[str, Any]] = []
    previous = loop.get_exception_handler()
    loop.set_exception_handler(lambda _loop, ctx: contexts.append(dict(ctx)))
    try:
        yield contexts
    finally:
        loop.set_exception_handler(previous)


def _callback_errors(contexts: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        ctx
        for ctx in contexts
        if "Exception in callback" in str(ctx.get("message", ""))
    ]


def _fail_append(
    log: Any, monkeypatch: pytest.MonkeyPatch, *, only: set[str] | None
) -> Callable[[], None]:
    """让落盘抛 OSError ENOSPC，返回「磁盘腾出空隙」的恢复开关。

    ``only`` 非空时只让这些**事件类型**的落盘失败（Message 记录与其余事件
    照常写）——复刻 issue 实录：回合提交成功、报告动作被同一故障击穿；None
    表示整条持久化路径失败（磁盘全满）。

    恢复走开关而不是撤销 monkeypatch：撤销会连带撤掉 autouse fixture 装上的
    会话目录 / provider 池隔离。
    """
    original = log.append
    state = {"broken": True}

    def guarded(records: list[dict[str, Any]], *args: Any, **kwargs: Any) -> None:
        if state["broken"]:
            for record in records:
                is_event = record.get("role") == "event"
                if only is None or (is_event and record.get("type") in only):
                    raise ENOSPC
        original(records, *args, **kwargs)

    monkeypatch.setattr(log, "append", guarded)

    def recover() -> None:
        state["broken"] = False

    return recover


async def _wait_until(pred: Callable[[], bool], timeout: float = 5.0) -> None:
    async def _inner() -> None:
        while not pred():
            await asyncio.sleep(0.005)

    await asyncio.wait_for(_inner(), timeout)


async def _settle() -> None:
    """让 done callback / call_soon 队列跑完。"""
    for _ in range(5):
        await asyncio.sleep(0)


async def _run_turn(agent: Any, content: str, request_id: str) -> None:
    """投递一条消息并等它被消费、回合收口（status 回 idle）。"""
    await agent.post(content, request_id=request_id)
    await _wait_until(lambda: agent.status == "idle" and not agent._inbox.has_pending)


def _types(events: list[Any]) -> list[str]:
    return [e.type for e in events]


def _turn_results(events: list[Any]) -> list[Any]:
    return [e for e in events if e.type == "turn_result"]


class TestReportPathIsUnfailable:
    """报告路径被同一故障击穿：回合仍以 error 干净收口、消费者不死。"""

    @pytest.mark.asyncio
    async def test_report_persist_failure_still_reports_and_keeps_worker(
        self, runtime: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """提交成功、上报落盘失败（ENOSPC）：error 收口 + worker 存活 + 后续可续。

        现场链（#187）：`turn_result` / `error` / `done` 都是 persist=true 的
        事实事件，要写同一本刚坏掉的 history.jsonl——严格发射在 except 块里
        再抛一次，逃出 `run_turn` 与 `_run`，消费者静默结束。
        """
        session = runtime.create_session()
        agent = session.agent
        recover = _fail_append(_persist_log(agent), monkeypatch, only=REPORT_TYPES)

        events: list[Any] = []
        event_bus.subscribe(events.append)
        worker = agent._worker

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await _run_turn(agent, "hello", "req-report-1")

        # ① 回合以 error 收口：上报动作虽落盘失败，仍必须到达客户端
        #    （前端靠它复位 working 态——"报告是记账，不是事实"）。
        assert _types(events)[-3:] == ["turn_result", "error", "done"], _types(events)
        results = _turn_results(events)
        assert results[-1].subtype == "error_during_execution", results[-1]
        assert results[-1].is_error is True, results[-1]
        # 故障文本可读（现场故障是磁盘打满）：报错面必须能定位到根因链。
        detail = results[-1].errors[-1]
        assert "No space left on device" in detail, detail
        assert any(
            "No space left on device" in e.message for e in events if e.type == "error"
        ), _types(events)

        # ② 状态复位：working 归位、可被下一次投递干净使用。
        assert agent._working is False
        assert agent.status == "idle"
        assert agent.turn_started_at is None

        # ③ worker 存活——且是**同一个**任务（它是被兜住的，不是被重建的）。
        assert agent._worker is worker
        assert not worker.done()

        # ④ 磁盘腾出空隙后，后续投递必须被消费（零僵尸态）。
        recover()
        events.clear()
        with patch.object(agent.model_provider, "generate", _mock_generate):
            await _run_turn(agent, "second", "req-report-2")

        results = _turn_results(events)
        assert results and results[-1].subtype == "success", _types(events)
        assert agent._worker is worker
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_whole_persist_outage_keeps_consuming(
        self, runtime: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """整条持久化路径失败（磁盘全满）：每一条投递都被消费、都不卡住。"""
        session = runtime.create_session()
        agent = session.agent
        recover = _fail_append(_persist_log(agent), monkeypatch, only=None)

        events: list[Any] = []
        event_bus.subscribe(events.append)
        worker = agent._worker

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await _run_turn(agent, "first", "req-full-1")
            await _run_turn(agent, "second", "req-full-2")

        # 两条投递各自收口为一次 error 回合（不是"投递进虚空"）。
        assert [r.subtype for r in _turn_results(events)] == [
            "error_during_execution",
            "error_during_execution",
        ], _types(events)
        assert agent._worker is worker and not worker.done()
        assert agent.status == "idle"
        assert agent._working is False

        # 磁盘恢复：会话自然继续工作。
        recover()
        events.clear()
        with patch.object(agent.model_provider, "generate", _mock_generate):
            await _run_turn(agent, "third", "req-full-3")

        results = _turn_results(events)
        assert results and results[-1].subtype == "success", _types(events)
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupt_and_shutdown_semantics_survive_the_outage(
        self, runtime: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """故障之后 interrupt / shutdown 语义不变（重建一次、关闭不复活）。"""
        session = runtime.create_session()
        agent = session.agent
        _fail_append(_persist_log(agent), monkeypatch, only=None)

        with patch.object(agent.model_provider, "generate", _mock_generate):
            await _run_turn(agent, "hello", "req-int-1")

        dropped = await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        assert dropped == []
        assert not agent._worker.done()

        await asyncio.wait_for(agent.shutdown(), timeout=5.0)
        await _settle()
        # 关闭是终局：收口不触发通用续期（不复活已关闭的 agent）。
        assert agent._worker.done()

    @pytest.mark.asyncio
    async def test_interrupt_report_survives_persist_failure(
        self, runtime: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """runtime 侧同族上报：打断已生效时上报落盘失败不得变成 500 + 事件丢失。

        `WingRuntime._emit_session_event` 发的是**已生效**动作的报告（打断 /
        回退 / 压缩 / 状态变更）——它走同一本 history.jsonl。上报失败若照旧
        抛出，`POST /api/session/interrupt` 会在打断已经生效的情况下返回 500，
        且 `InterruptedEvent`（含被放弃消息的 request_id）永不下发。
        """
        session = runtime.create_session()
        agent = session.agent
        _fail_append(_persist_log(agent), monkeypatch, only=None)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        await asyncio.wait_for(
            runtime.interrupt_session(session.session_id), timeout=5.0
        )

        interrupted = [e for e in events if e.type == "interrupted"]
        assert len(interrupted) == 1, _types(events)
        assert interrupted[0].dropped_request_ids == [], interrupted[0]
        assert not agent._worker.done()
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_turn_failure_survives_exception_in_report_chain(
        self, runtime: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """双故障叠加（阶梯耗尽 + 落盘故障）：打断仍有界返回、notice 仍下发。

        极端形态：worker 连取消也不响应（`_report_undead_worker` 的 notice 是
        **错误路径里的副作用**），同时落盘整体失败。收口链上的任何一环都不
        允许把 interrupt 拖住或炸掉——notice 事件此刻一条都不能少。
        """
        import wing.agent.core as core

        monkeypatch.setattr(core, "_INTERRUPT_WAIT_SECONDS", 0.02)

        stop = asyncio.Event()
        started = asyncio.Event()

        async def _swallow_all(_loop: Any) -> None:
            """替身 `ReActLoop.run_turn`（类属性：多一个 self）。"""
            started.set()
            while True:
                try:
                    await asyncio.sleep(30)
                except asyncio.CancelledError:
                    if stop.is_set():
                        raise  # 收尾：放行取消，worker 正常终止
                    continue

        # 在 agent 创建**之前**打补丁：worker 的第一次 `run_turn` 就是它——
        # 不靠投递消息把状态推进到那一步（那会与 interrupt 的入口清理竞态）。
        monkeypatch.setattr(ReActLoop, "run_turn", _swallow_all)
        session = runtime.create_session()
        agent = session.agent
        _fail_append(_persist_log(agent), monkeypatch, only=None)

        worker = agent._worker
        await asyncio.wait_for(started.wait(), timeout=2.0)

        events: list[Any] = []
        event_bus.subscribe(events.append)

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)

        notices = [e for e in events if e.type == "notice"]
        assert notices and notices[-1].level == "error", _types(events)
        # 阶梯耗尽：保留原 worker（绝不重建第二个）。
        assert agent._worker is worker and not worker.done()

        # 收尾：先放掉吞取消的 worker，再走 shutdown（不污染 loop）。
        stop.set()
        worker.cancel()
        await asyncio.gather(worker, return_exceptions=True)
        await agent.shutdown()


class TestWorkerRenewal:
    """worker 终局续期：非意图性异常结束 → 自动重建；意图性收口不抢建。"""

    @pytest.mark.asyncio
    async def test_worker_escaping_all_guards_is_renewed(
        self, runtime: Any, wing_warnings: list[str]
    ) -> None:
        """worker 以异常逃出主循环：自动重建消费者，后续投递仍被消费。"""
        session = runtime.create_session()
        agent = session.agent
        dead = agent._worker

        with patch.object(agent.model_provider, "generate", _boom_generate):
            await agent.post("trigger", request_id="req-renew-1")
            await _wait_until(dead.done)
            await _wait_until(lambda: agent._worker is not dead)

        assert not agent._worker.done()
        assert any("重建消费者" in m for m in wing_warnings), wing_warnings

        # 后续投递仍被消费（重建出的消费者真的在消费）。
        events: list[Any] = []
        event_bus.subscribe(events.append)
        with patch.object(agent.model_provider, "generate", _mock_generate):
            await _run_turn(agent, "after renewal", "req-renew-2")

        results = _turn_results(events)
        assert results and results[-1].subtype == "success", _types(events)
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_cancelled_worker_is_not_renewed(self, runtime: Any) -> None:
        """取消是意图性收口（interrupt / shutdown / 测试停机）：不触发续期。

        守两件事：没有第二个消费者；**回调本身不出错**——已取消的任务上取
        `exception()` 会抛 `CancelledError`，判据顺序写反就变成 loop 的
        "Exception in callback"（续期静默失效），而"没被替换"的断言照样绿。
        变异守门：删掉 `_on_worker_done` 的 `worker.cancelled()` 判据后本用例
        转红（回调异常被 loop 记下）。
        """
        session = runtime.create_session()
        agent = session.agent
        worker = agent._worker

        with _capture_loop_callback_errors() as contexts:
            worker.cancel()
            await asyncio.gather(worker, return_exceptions=True)
            await _settle()

        assert agent._worker is worker
        assert worker.done()
        assert _callback_errors(contexts) == [], contexts
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_kept_worker_dying_with_exception_rebuilds_exactly_one_consumer(
        self, runtime: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """双回调形态：阶梯放手保留后以异常死亡 → 活着的消费者恰好一个。

        生产中唯一让两条续期回调**同时登记**的路径：阶梯放手保留的 worker
        （`_arm_worker_renewal` 的 `_renew` 已登记）随后以**非取消**方式死亡
        （异常逃逸）——此时通用续期（`_on_worker_done`）也会触发，防重复全靠
        两边的 `self._worker is worker` 判据。只看 `agent._worker` 抓不到孤儿
        消费者（它照样在抢同一个 inbox），所以这里数**活着的消费者任务**。
        变异守门：删掉任一道身份判据 → 两个活消费者（本用例转红）。
        """
        import wing.agent.core as core

        monkeypatch.setattr(core, "_INTERRUPT_WAIT_SECONDS", 0.02)

        started = asyncio.Event()

        async def _swallow_then_die(_loop: Any) -> None:
            started.set()
            for _ in range(3):
                try:
                    await asyncio.sleep(30)
                except asyncio.CancelledError:
                    pass
            await asyncio.sleep(0.2)  # 阶梯之外才死：interrupt 已放手并登记续期
            raise WorkerEscape("dies after being kept")

        # 在 agent 创建之前打补丁：worker 的第一次 `run_turn` 就是它（否则补丁
        # 落不进在途的那次调用，时序要赌消息投递与 interrupt 的入口清理谁先）。
        monkeypatch.setattr(ReActLoop, "run_turn", _swallow_then_die)
        session = runtime.create_session()
        agent = session.agent
        spawned = _record_spawns(agent, monkeypatch)
        kept = spawned[0]
        # 等 worker 真正进入吞取消循环：对**未启动**的协程 cancel，CancelledError
        # 在协程体执行前抛出、body 无从吞掉（阶梯一次就杀掉它）。
        await asyncio.wait_for(started.wait(), timeout=2.0)

        await asyncio.wait_for(agent.interrupt(), timeout=5.0)
        assert agent._worker is kept  # 先保留（绝不重建第二个）

        await _wait_until(kept.done, timeout=3.0)
        await _settle()

        live = [task for task in spawned if not task.done()]
        assert len(live) == 1, [repr(t) for t in spawned]
        assert live[0] is agent._worker
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_renewal_gate_stays_closed_after_shutdown(self, runtime: Any) -> None:
        """`_closing` 闸门挡住通用续期：关闭之后 worker 迟死也不复活。

        现场是「关闭与 worker 死亡竞态」——这里直接构造那个终局（与
        `test_interrupt_ladder._replace_worker` 同款手法），不赌调度。变异
        守门：删掉 `_on_worker_done` 里的 `self._closing` 判断后本用例转红
        （`agent._worker` 会变成一个活着的消费者）。
        """
        session = runtime.create_session()
        agent = session.agent
        await asyncio.wait_for(agent.shutdown(), timeout=5.0)  # 闸门置位

        with patch.object(agent.model_provider, "generate", _boom_generate):
            late = agent._spawn_worker()  # 关闭之后才终止的消费者（竞态现场）
            agent._worker = late
            await agent.post("nobody should wake up", request_id="req-closed")
            await _wait_until(late.done)
            await _settle()

        assert agent._worker is late
        assert late.done()
