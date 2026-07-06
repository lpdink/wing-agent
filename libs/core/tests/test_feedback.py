"""测试 need_feedback 反馈路径。"""

import asyncio

import pytest

from wing.request_context import set_request_context, reset_request_context
from wing.runtime import WingRuntime


@pytest.fixture
def runtime():
    yield WingRuntime()


@pytest.mark.asyncio
async def test_feedback_basic_path(runtime: WingRuntime):
    """需要反馈时，post() 到 agent 被路由到 _inbox_feedback。"""
    session = runtime.create_session(client_id=None)
    agent = session.agent

    # 模拟 bash 工具的 _handle_dangerous_command
    agent.state.set("need_feedback", True)

    async def receiver():
        return await agent._inbox_feedback.get()

    task = asyncio.create_task(receiver())
    await asyncio.sleep(0.01)

    # 用户发送确认（走 SM._post → agent.post
    token = set_request_context(client_id="tui", session_id=session.session_id)
    try:
        await runtime.sm._post("y, go ahead", session_id=session.session_id)
    finally:
        reset_request_context(token)

    feedback = await asyncio.wait_for(task, timeout=1.0)
    assert feedback == "y, go ahead"


@pytest.mark.asyncio
async def test_feedback_consumer(runtime: WingRuntime):
    """模拟 bash 工具完整流程：设置 need_feedback→等反馈→消费→清理。"""
    session = runtime.create_session(client_id=None)
    agent = session.agent

    agent.state.set("need_feedback", True)

    async def bash_simulator():  # 模拟 bash 工具的等反馈循环
        while True:
            fb = await agent._inbox_feedback.get()
            if fb.strip()[:1].lower() in ("y", "n"):
                return fb

    task = asyncio.create_task(bash_simulator())
    await asyncio.sleep(0.01)

    # 先发无效反馈
    token = set_request_context(client_id="tui", session_id=session.session_id)
    try:
        await runtime.sm._post("maybe", session_id=session.session_id)
        # 再发有效反馈
        await runtime.sm._post("y", session_id=session.session_id)
    finally:
        reset_request_context(token)

    result = await asyncio.wait_for(task, timeout=1.0)
    assert result == "y"
