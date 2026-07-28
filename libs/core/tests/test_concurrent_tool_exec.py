"""Tests for concurrent tool execution in WingAgent.exec_tool_calls.

Verifies:
  - Multiple tool calls execute concurrently (total time ≈ max, not sum)
  - Result order matches input tool call order regardless of completion order
  - Error isolation: one tool failure doesn't affect others
  - Single tool call behavior unchanged
"""

from __future__ import annotations

import asyncio
import time
from typing import Any

import pytest

from wing.event_bus import event_bus
from wing.schema import Tool, ToolCall, ToolParam


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


def _make_tool(name: str, fn) -> Tool:
    """创建一个最小化的 Tool 对象。"""
    return Tool(
        name=name,
        description=f"Test tool: {name}",
        params=[ToolParam(name="input", type="string", description="input")],
        function=fn,
    )


def _make_tool_call(tool_id: str, name: str, arguments: dict | None = None) -> ToolCall:
    return ToolCall(id=tool_id, name=name, arguments=arguments or {"input": "test"})


class TestConcurrentExecution:
    """验证多个工具调用并发执行。"""

    @pytest.mark.asyncio
    async def test_concurrent_timing(self, runtime: Any):
        """多个慢工具并发执行，总耗时接近最慢工具而非串行总和。"""
        session = runtime.create_session()
        agent = session.agent

        delay = 0.3  # 每个工具 sleep 300ms

        async def slow_tool_a(input: str = "") -> str:
            await asyncio.sleep(delay)
            return "result-a"

        async def slow_tool_b(input: str = "") -> str:
            await asyncio.sleep(delay)
            return "result-b"

        async def slow_tool_c(input: str = "") -> str:
            await asyncio.sleep(delay)
            return "result-c"

        agent._tools = {
            "SlowA": _make_tool("SlowA", slow_tool_a),
            "SlowB": _make_tool("SlowB", slow_tool_b),
            "SlowC": _make_tool("SlowC", slow_tool_c),
        }

        tool_calls = [
            _make_tool_call("call-1", "SlowA"),
            _make_tool_call("call-2", "SlowB"),
            _make_tool_call("call-3", "SlowC"),
        ]

        start = time.monotonic()
        results = await agent.exec_tool_calls(tool_calls)
        elapsed = time.monotonic() - start

        # 串行需要 ~0.9s，并发应 ~0.3s；用 0.6s 作为阈值留足余量
        assert elapsed < delay * 2, (
            f"Expected concurrent (~{delay}s), got {elapsed:.2f}s"
        )
        assert len(results) == 3
        assert results[0].content == "result-a"
        assert results[1].content == "result-b"
        assert results[2].content == "result-c"

        await agent.shutdown()


class TestResultOrderPreservation:
    """验证返回结果顺序严格等于输入 tool call 顺序。"""

    @pytest.mark.asyncio
    async def test_order_preserved_despite_different_completion_times(
        self, runtime: Any
    ):
        """后发先至的工具结果仍按原始顺序排列。"""
        session = runtime.create_session()
        agent = session.agent

        async def slow_tool(input: str = "") -> str:
            await asyncio.sleep(0.3)
            return "slow-result"

        async def fast_tool(input: str = "") -> str:
            await asyncio.sleep(0.01)
            return "fast-result"

        agent._tools = {
            "Slow": _make_tool("Slow", slow_tool),
            "Fast": _make_tool("Fast", fast_tool),
        }

        # Slow 在前，Fast 在后；Fast 会先完成
        tool_calls = [
            _make_tool_call("call-slow", "Slow"),
            _make_tool_call("call-fast-1", "Fast"),
            _make_tool_call("call-fast-2", "Fast"),
        ]

        results = await agent.exec_tool_calls(tool_calls)

        assert len(results) == 3
        # 顺序必须与输入一致：Slow, Fast, Fast
        assert results[0].tool_call_id == "call-slow"
        assert results[0].content == "slow-result"
        assert results[1].tool_call_id == "call-fast-1"
        assert results[1].content == "fast-result"
        assert results[2].tool_call_id == "call-fast-2"
        assert results[2].content == "fast-result"

        await agent.shutdown()


class TestErrorIsolation:
    """验证单个工具失败不影响其他工具执行。"""

    @pytest.mark.asyncio
    async def test_one_failure_does_not_affect_others(self, runtime: Any):
        """一个工具抛异常，其他工具正常返回结果。"""
        session = runtime.create_session()
        agent = session.agent

        async def good_tool(input: str = "") -> str:
            await asyncio.sleep(0.05)
            return "good-result"

        async def bad_tool(input: str = "") -> str:
            await asyncio.sleep(0.01)
            raise RuntimeError("boom")

        agent._tools = {
            "Good": _make_tool("Good", good_tool),
            "Bad": _make_tool("Bad", bad_tool),
        }

        tool_calls = [
            _make_tool_call("call-good-1", "Good"),
            _make_tool_call("call-bad", "Bad"),
            _make_tool_call("call-good-2", "Good"),
        ]

        results = await agent.exec_tool_calls(tool_calls)

        assert len(results) == 3
        # 第一个 Good 正常
        assert results[0].tool_call_id == "call-good-1"
        assert results[0].content == "good-result"
        # Bad 返回错误信息
        assert results[1].tool_call_id == "call-bad"
        assert "boom" in results[1].content
        # 第二个 Good 不受影响
        assert results[2].tool_call_id == "call-good-2"
        assert results[2].content == "good-result"

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_unknown_tool_does_not_affect_others(self, runtime: Any):
        """未注册工具返回错误，不影响其他工具。"""
        session = runtime.create_session()
        agent = session.agent

        async def good_tool(input: str = "") -> str:
            return "good-result"

        agent._tools = {
            "Good": _make_tool("Good", good_tool),
        }

        tool_calls = [
            _make_tool_call("call-good", "Good"),
            _make_tool_call("call-unknown", "NonExistent"),
        ]

        results = await agent.exec_tool_calls(tool_calls)

        assert len(results) == 2
        assert results[0].content == "good-result"
        assert (
            "unknown tool" in results[1].content.lower()
            or "NonExistent" in results[1].content
        )

        await agent.shutdown()


class TestSingleToolCall:
    """验证单个工具调用行为不变。"""

    @pytest.mark.asyncio
    async def test_single_tool_call(self, runtime: Any):
        """单个工具调用正常执行并返回结果。"""
        session = runtime.create_session()
        agent = session.agent

        async def simple_tool(input: str = "") -> str:
            return f"echo: {input}"

        agent._tools = {
            "Simple": _make_tool("Simple", simple_tool),
        }

        tool_calls = [_make_tool_call("call-1", "Simple", {"input": "hello"})]

        results = await agent.exec_tool_calls(tool_calls)

        assert len(results) == 1
        assert results[0].tool_call_id == "call-1"
        assert results[0].role == "tool"
        assert results[0].content == "echo: hello"

        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_empty_tool_calls(self, runtime: Any):
        """空 tool call 列表返回空结果。"""
        session = runtime.create_session()
        agent = session.agent

        results = await agent.exec_tool_calls([])

        assert results == []

        await agent.shutdown()
