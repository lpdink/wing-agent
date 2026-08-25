# tests/test_malformed_tool_args.py
"""非法工具参数软着陆测试。

笨模型吐出的 tool args JSON 可能非法（如尾逗号）。正确行为（对齐早先
的运行时语义）：不抛异常、不整轮重试丢弃已生成内容，而是把这次工具
调用当作一次正常的失败执行——合成错误结果回灌模型，让它下一轮自纠。

锁定行为：
- parse_tool_args 永不抛（见 test_process_tool_deltas.py）
- ToolExecutor 见 arguments_error 短路：不执行工具、不走 hook，
  合成保留现场的错误结果（success=False）
- 整轮 turn 正常提交：assistant（text + tool_use）+ 错误 tool result 入库，
  loop 继续下一轮给模型自纠机会
- input_error 经 Message.tool_calls 派生透传，pydantic round-trip 无损
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from wing.agent import Inbound
from wing.agent.event_sink import AgentEventSink
from wing.agent.tool_executor import ToolExecutor
from wing.event import DoneEvent, ToolCallResultEvent
from wing.event_bus import event_bus
from wing.provider.base import parse_tool_args
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    TextBlock,
    Tool,
    ToolCall,
    ToolParam,
    ToolUseBlock,
)


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
    from wing.runtime import WingRuntime

    return WingRuntime()


def _make_tool(name: str, fn) -> Tool:
    return Tool(
        name=name,
        description=f"Test tool: {name}",
        params=[ToolParam(name="input", type="string", description="input")],
        function=fn,
    )


class TestExecutorShortCircuit:
    """ToolExecutor 对 arguments_error 调用短路：不执行、合成错误结果。"""

    @pytest.mark.asyncio
    async def test_malformed_call_not_executed_returns_error_result(self):
        executed: list[dict] = []

        def echo(input: str = "") -> str:
            executed.append({"input": input})
            return "ok"

        executor = ToolExecutor(AgentEventSink(session_id="test"))
        executor.set_tools({"Echo": _make_tool("Echo", echo)})

        events: list[Any] = []
        event_bus.subscribe(events.append)

        _, err = parse_tool_args('{"input": "hello",}')
        assert err is not None
        tc = ToolCall(id="call-bad", name="Echo", arguments={}, arguments_error=err)

        results = await executor.execute([tc], "test-model")

        # 工具未被执行
        assert executed == []
        # 合成错误结果：保留现场（错误详情 + 原始文本）
        assert len(results) == 1
        msg = results[0]
        assert msg.role == "tool"
        assert msg.tool_call_id == "call-bad"
        assert msg.content is not None
        assert "NOT executed" in msg.content
        assert '{"input": "hello",}' in msg.content
        # 结果事件 success=False（metrics 计为失败工具调用）
        result_events = [e for e in events if isinstance(e, ToolCallResultEvent)]
        assert len(result_events) == 1
        assert result_events[0].tool_success is False
        assert result_events[0].tool_call_id == "call-bad"


class TestSchemaPropagation:
    """input_error ↔ arguments_error 派生透传与持久化无损。"""

    def test_message_tool_calls_propagates_input_error(self):
        msg = Message(
            role="assistant",
            content_blocks=[
                ToolUseBlock(id="t1", name="X", input={}, input_error="boom")
            ],
        )
        calls = msg.tool_calls
        assert calls is not None and len(calls) == 1
        assert calls[0].arguments == {}
        assert calls[0].arguments_error == "boom"

    def test_input_error_round_trip_persistence(self):
        m = Message(
            role="assistant",
            content_blocks=[
                ToolUseBlock(id="x", name="Read", input={}, input_error="boom")
            ],
        )
        m2 = Message.model_validate(m.model_dump())
        blocks = m2.content_blocks
        assert blocks is not None
        assert isinstance(blocks[0], ToolUseBlock)
        assert blocks[0].input_error == "boom"
        assert m2.tool_calls is not None
        assert m2.tool_calls[0].arguments_error == "boom"


class TestFullTurnMalformedArgs:
    """整轮行为：非法参数的 turn 正常提交，模型收到错误并继续。"""

    @pytest.mark.asyncio
    async def test_turn_committed_and_loop_continues(self, runtime, monkeypatch):
        session = runtime.create_session()
        agent = session.agent

        executed: list[dict] = []

        async def echo(input: str = "") -> str:
            executed.append({"input": input})
            return "ok"

        agent._executor._tools = {"Echo": _make_tool("Echo", echo)}

        events: list[Any] = []
        event_bus.subscribe(events.append)

        raw_bad = '{"input": "hello",}'  # 尾逗号——笨模型典型错误
        _, args_error = parse_tool_args(raw_bad)
        assert args_error is not None

        gen_count = 0

        async def _generate(*args: Any, **kwargs: Any):
            nonlocal gen_count
            gen_count += 1
            if gen_count == 1:
                # 第一轮：text + 非法参数的 tool call（provider 已容错解析）
                yield LLMResponse(
                    content_blocks=[
                        TextBlock(text="calling"),
                        ToolUseBlock(
                            id="call-bad", name="Echo", input={}, input_error=args_error
                        ),
                    ],
                    usage=LLMUsage(prompt_tokens=10, completion_tokens=5),
                )
            else:
                # 第二轮：模型收到错误结果后重新生成，正常收尾
                yield LLMResponse(
                    content_blocks=[TextBlock(text="done")],
                    usage=LLMUsage(prompt_tokens=12, completion_tokens=3),
                )

        monkeypatch.setattr(agent.model_provider, "generate", _generate)

        agent._inbox._queue.put_nowait(
            Inbound(message=Message(role="user", content="go"))
        )

        async def _wait_done():
            while not any(isinstance(e, DoneEvent) for e in events):
                await asyncio.sleep(0.005)

        await asyncio.wait_for(_wait_done(), 5.0)

        # ── turn 未被丢弃：assistant + 错误 tool result 沿正常路径入库 ──
        chain = agent.context_manager.get_context_window()
        assert [m.role for m in chain[:4]] == ["user", "assistant", "tool", "assistant"]
        assistant_msg = chain[1]
        assert assistant_msg.content == "calling"  # text/thinking 未丢
        assert assistant_msg.tool_calls is not None
        assert assistant_msg.tool_calls[0].arguments_error == args_error

        tool_msg = chain[2]
        assert tool_msg.tool_call_id == "call-bad"
        assert tool_msg.content is not None
        assert "NOT executed" in tool_msg.content
        assert raw_bad in tool_msg.content  # 现场保留，模型可见

        # 工具从未真正执行；loop 继续给了模型第二次机会
        assert executed == []
        assert gen_count == 2
        assert chain[3].content == "done"

        await agent.shutdown()
