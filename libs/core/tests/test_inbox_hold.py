"""Inbox 取出口闸门（`hold` / `release`）与 `WingAgent.compact` 的准入 / 互斥 / 取消。

被守的语义（`agent/inbox.py` + `agent/core.py`）：

- `hold()` 期间 `get()` **不把消息交出去**——消息留在队列里（`has_pending` 为真），
  压缩结束 `release()` 后按原序交付；hold 边界上已经取到手的条目原样放回队首。
- 手动压缩是会话级独占操作：非 idle（在跑 turn / 等 ask）/ 有排队输入 / 已有压缩在飞
  一律拒绝（`SessionBusyError`），不排队等待。
- 压缩被 interrupt 取消时链零变更（压缩只在最后一个 await 之后写链），且 hold 一定
  被释放（下一次 `get()` 照常交付）。
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from typing import Any

import pytest
import pytest_asyncio

from wing.agent import WingAgent
from wing.agent.inbox import Inbox
from wing.schema import LLMResponse, LLMUsage, Message
from wing.session import SessionBusyError

pytestmark = pytest.mark.usefixtures("cleanup_event_bus")


async def _wait_until(pred: Any, timeout: float = 2.0) -> None:
    expires = asyncio.get_running_loop().time() + timeout
    while not pred():
        if asyncio.get_running_loop().time() >= expires:
            raise AssertionError("condition not met within deadline")
        await asyncio.sleep(0.005)


# ── Inbox 闸门 ────────────────────────────────────────


@pytest.mark.asyncio
async def test_get_blocks_while_held() -> None:
    inbox = Inbox()
    inbox.hold()
    await inbox.post("hello")

    waiting = asyncio.create_task(inbox.get())
    await asyncio.sleep(0.02)
    assert not waiting.done(), "hold 期间不得交付任何输入"
    assert inbox.has_pending is True, "消息必须留在队列里（逐出判定据此钉住会话）"

    inbox.release()
    item = await asyncio.wait_for(waiting, 1.0)
    assert item.message.content == "hello"
    assert inbox.has_pending is False


@pytest.mark.asyncio
async def test_hold_after_getter_is_waiting_keeps_order() -> None:
    """hold 落在「已经挂在 get() 上」的窗口里：取到手的条目要放回队首。"""
    inbox = Inbox()
    waiting = asyncio.create_task(inbox.get())
    await asyncio.sleep(0.02)  # 让它停在 queue.get() 上

    inbox.hold()
    await inbox.post("first")
    await inbox.post("second")
    await asyncio.sleep(0.02)
    assert not waiting.done()
    assert inbox.has_pending is True

    inbox.release()
    first = await asyncio.wait_for(waiting, 1.0)
    assert first.message.content == "first", "顺序不得被闸门打乱"
    assert inbox.drain()[0].message.content == "second"


@pytest.mark.asyncio
async def test_cancel_while_held_does_not_lose_the_item() -> None:
    """等闸门的 get() 被取消：条目仍在队列里（不丢输入）。"""
    inbox = Inbox()
    inbox.hold()
    await inbox.post("survives")

    waiting = asyncio.create_task(inbox.get())
    await asyncio.sleep(0.02)
    waiting.cancel()
    with pytest.raises(asyncio.CancelledError):
        await waiting

    inbox.release()
    item = await asyncio.wait_for(inbox.get(), 1.0)
    assert item.message.content == "survives"


# ── WingAgent.compact：准入 / 互斥 / 取消 ─────────────


class _BlockingProvider:
    """压缩调用可控的 provider 替身：`release` 之前不返回。"""

    name = "default"

    def __init__(self) -> None:
        self.entered = asyncio.Event()
        self.release = asyncio.Event()
        self.calls: list[list[Message]] = []

    async def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
        accumulator: object = None,
        options: object = None,
    ) -> AsyncIterator[LLMResponse]:
        self.calls.append(list(messages))
        self.entered.set()
        await self.release.wait()
        yield LLMResponse(
            content="<summary>compressed</summary>",
            usage=LLMUsage(prompt_tokens=100, completion_tokens=10),
        )


@pytest_asyncio.fixture
async def rig(
    install_provider: Any,
) -> AsyncIterator[tuple[WingAgent, _BlockingProvider]]:
    """真实 runtime 装配的 agent + 可控 provider（装进共享池，压缩调用会走到它）。"""
    from wing.runtime import WingRuntime

    provider = _BlockingProvider()
    install_provider(provider)
    session = WingRuntime().create_session()
    agent = session.agent
    try:
        yield agent, provider
    finally:
        await agent.shutdown()


@pytest_asyncio.fixture
async def agent(rig: tuple[WingAgent, _BlockingProvider]) -> WingAgent:
    return rig[0]


@pytest.mark.asyncio
async def test_compact_rejects_busy_session(agent: WingAgent) -> None:
    agent._set_working(True)
    try:
        with pytest.raises(SessionBusyError):
            await agent.compact()
    finally:
        agent._set_working(False)
    assert agent.compacting is False


@pytest.mark.asyncio
async def test_compact_rejects_pending_input(agent: WingAgent) -> None:
    agent._inbox.hold()  # 把消息摁在队列里，模拟「已发送、worker 还没取走」
    try:
        await agent.post("queued")
        with pytest.raises(SessionBusyError):
            await agent.compact()
    finally:
        agent._inbox.release()
        await agent.interrupt()


@pytest.mark.asyncio
async def test_compact_rejects_second_compaction(
    agent: WingAgent, rig: tuple[WingAgent, _BlockingProvider]
) -> None:
    provider = rig[1]
    provider.release.set()  # 第一次立刻完成
    running = asyncio.create_task(agent.compact())
    await asyncio.wait_for(provider.entered.wait(), 1.0)
    assert agent.compacting is True
    with pytest.raises(SessionBusyError):
        await agent.compact()
    await asyncio.wait_for(running, 2.0)
    assert agent.compacting is False


@pytest.mark.asyncio
async def test_compaction_holds_input_until_it_finishes(
    agent: WingAgent, rig: tuple[WingAgent, _BlockingProvider]
) -> None:
    """压缩在飞时投递的消息不驱动 turn：压缩结束才开始处理。"""
    provider = rig[1]
    running = asyncio.create_task(agent.compact())
    await asyncio.wait_for(provider.entered.wait(), 1.0)

    await agent.post("queued during compaction")
    await asyncio.sleep(0.05)
    assert agent.status == "idle", "压缩期间不得开始新一轮"
    assert agent._inbox.has_pending is True

    provider.release.set()
    await asyncio.wait_for(running, 2.0)
    # 压缩结束后 worker 才拿到它（随后这一轮会失败——测试没配主调用剧本，
    # 但「取走」这个事实已经由 has_pending 翻转证明）。
    await _wait_until(lambda: agent._inbox.has_pending is False)
    await agent.interrupt()


@pytest.mark.asyncio
async def test_interrupt_cancels_compaction_without_touching_chain(
    agent: WingAgent, rig: tuple[WingAgent, _BlockingProvider]
) -> None:
    provider = rig[1]
    agent.context_manager.add_message(Message(role="user", content="alpha"))
    before = [m.content for m in agent.context_manager.get_context_window()]

    running = asyncio.create_task(agent.compact())
    await asyncio.wait_for(provider.entered.wait(), 1.0)
    await agent.interrupt()

    with pytest.raises(SessionBusyError):
        await asyncio.wait_for(running, 5.0)
    assert agent.compacting is False
    assert [m.content for m in agent.context_manager.get_context_window()] == before
    assert agent._inbox.held is False, "取消后必须释放取出口闸门"
