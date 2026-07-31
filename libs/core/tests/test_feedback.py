"""测试 feedback 寻址路径（feedback addressing by tool_call_id）。

PR #33 将工具调用改为并发执行后，旧的「单共享队列 + need_feedback 布尔」
设计会让并发的 feedback 等待者互相饿死。现在 feedback 严格寻址：
每个等待者注册一个以 tool_call_id 为 key 的 Future，只有携带对应 key
的回复才会 resolve 它；无 key 的消息（普通用户输入、Explorer 等
内部通知）一律进 inbox。
"""

import asyncio

import pytest

from wing.agent import WingAgent, _current_tool_call_id
from wing.event import AskEvent
from wing.request_context import set_request_context, reset_request_context
from wing.runtime import WingRuntime


@pytest.fixture
def runtime():
    yield WingRuntime()


async def _ask(agent: WingAgent, tc_id: str, timeout: float = 1.0) -> str:
    """模拟工具等待反馈：设置工具调用 context（如同 exec_tool_calls），
    经 ask_feedback() 注册 waiter 并等待定向回复。"""
    token = _current_tool_call_id.set(tc_id)
    try:
        return await agent.ask_feedback(
            AskEvent(question="proceed?", choices=["y", "n"], required=True),
            timeout=timeout,
        )
    finally:
        _current_tool_call_id.reset(token)


@pytest.mark.asyncio
async def test_addressed_feedback_basic_path(runtime: WingRuntime):
    """携带 tool_call_id 的 post 经完整链路 resolve 对应 waiter。"""
    session = runtime.create_session()
    agent = session.agent
    # 停掉 worker：本文件只验证 post 路由与 waiter 机制，
    # 避免 worker 消费 inbox 中的测试消息触发 LLM 调用
    agent._worker.cancel()

    task = asyncio.create_task(_ask(agent, "tc_1"))
    await asyncio.sleep(0.01)
    assert agent.status == "waiting"

    token = set_request_context(client_id="tui", session_id=session.session_id)
    try:
        await runtime.sm._post(
            "y, go ahead", session_id=session.session_id, tool_call_id="tc_1"
        )
    finally:
        reset_request_context(token)

    feedback = await asyncio.wait_for(task, timeout=1.0)
    assert feedback == "y, go ahead"
    assert agent.status != "waiting"
    assert not agent._feedback_waiters


@pytest.mark.asyncio
async def test_concurrent_waiters_addressed_out_of_order(runtime: WingRuntime):
    """两个并发 waiter：乱序定向应答各得其所。

    PR #33 后的回归场景——旧设计中第一个消费者清掉 need_feedback flag，
    第二条回复被误路由进 inbox，第二个 waiter 必然饿死超时。
    """
    session = runtime.create_session()
    agent = session.agent
    # 停掉 worker：本文件只验证 post 路由与 waiter 机制，
    # 避免 worker 消费 inbox 中的测试消息触发 LLM 调用
    agent._worker.cancel()

    task1 = asyncio.create_task(_ask(agent, "tc_1"))
    task2 = asyncio.create_task(_ask(agent, "tc_2"))
    await asyncio.sleep(0.01)
    assert set(agent._feedback_waiters) == {"tc_1", "tc_2"}
    assert agent.status == "waiting"

    token = set_request_context(client_id="tui", session_id=session.session_id)
    try:
        # 故意先回复第二个 waiter
        await runtime.sm._post("n", session_id=session.session_id, tool_call_id="tc_2")
        await runtime.sm._post("y", session_id=session.session_id, tool_call_id="tc_1")
    finally:
        reset_request_context(token)

    assert await asyncio.wait_for(task1, timeout=1.0) == "y"
    assert await asyncio.wait_for(task2, timeout=1.0) == "n"
    assert not agent._feedback_waiters


@pytest.mark.asyncio
async def test_unaddressed_message_goes_to_inbox_not_waiter(runtime: WingRuntime):
    """无 tool_call_id 的消息即使存在 waiter 也进 inbox。

    回归用例：后台 Explorer 完成通知经 post() 投递时不带 id，
    旧设计中会被 need_feedback flag 误导成「用户回答」被等待中的工具吃掉。
    """
    session = runtime.create_session()
    agent = session.agent
    # 停掉 worker：本文件只验证 post 路由与 waiter 机制，
    # 避免 worker 消费 inbox 中的测试消息触发 LLM 调用
    agent._worker.cancel()

    task = asyncio.create_task(_ask(agent, "tc_1"))
    await asyncio.sleep(0.01)

    # 模拟后台 Explorer 完成通知（无 tool_call_id）
    await agent.post("[Explorer] Task 'scan' completed. Results saved to: /tmp/r.md")
    await asyncio.sleep(0.01)

    # waiter 未被消费
    assert "tc_1" in agent._feedback_waiters
    assert not task.done()
    # 通知进了 inbox
    inbound = agent._inbox.get_nowait()
    assert (inbound.message.content or "").startswith("[Explorer]")

    # 定向回复依然正常 resolve waiter
    await agent.post("y", tool_call_id="tc_1")
    assert await asyncio.wait_for(task, timeout=1.0) == "y"


@pytest.mark.asyncio
async def test_stale_tool_call_id_falls_through_to_inbox(runtime: WingRuntime):
    """定向到已失效（超时/不存在）的 waiter id：消息落 inbox，不丢失。"""
    session = runtime.create_session()
    agent = session.agent
    # 停掉 worker：本文件只验证 post 路由与 waiter 机制，
    # 避免 worker 消费 inbox 中的测试消息触发 LLM 调用
    agent._worker.cancel()

    await agent.post("late reply", tool_call_id="tc_gone")
    inbound = agent._inbox.get_nowait()
    assert inbound.message.content == "late reply"


@pytest.mark.asyncio
async def test_feedback_timeout_unregisters_waiter(runtime: WingRuntime):
    """超时后 waiter 自动注销，status 恢复。"""
    session = runtime.create_session()
    agent = session.agent
    # 停掉 worker：本文件只验证 post 路由与 waiter 机制，
    # 避免 worker 消费 inbox 中的测试消息触发 LLM 调用
    agent._worker.cancel()

    with pytest.raises(asyncio.TimeoutError):
        await _ask(agent, "tc_1", timeout=0.05)

    assert "tc_1" not in agent._feedback_waiters
    assert agent.status == "idle"

    # 超时后该 id 的迟到回复落 inbox
    await agent.post("better late than never", tool_call_id="tc_1")
    inbound = agent._inbox.get_nowait()
    assert inbound.message.content == "better late than never"


@pytest.mark.asyncio
async def test_interrupt_cancels_waiters(runtime: WingRuntime):
    """interrupt 取消所有 waiter 并清空注册表。"""
    session = runtime.create_session()
    agent = session.agent
    # 停掉 worker：本文件只验证 post 路由与 waiter 机制，
    # 避免 worker 消费 inbox 中的测试消息触发 LLM 调用
    agent._worker.cancel()

    task1 = asyncio.create_task(_ask(agent, "tc_1"))
    task2 = asyncio.create_task(_ask(agent, "tc_2"))
    await asyncio.sleep(0.01)
    assert len(agent._feedback_waiters) == 2

    await agent.interrupt()
    assert not agent._feedback_waiters

    with pytest.raises(asyncio.CancelledError):
        await task1
    with pytest.raises(asyncio.CancelledError):
        await task2
