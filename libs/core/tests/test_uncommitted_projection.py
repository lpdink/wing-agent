"""未提交内容投影测试。

未提交内容（当前 turn 已生成但未提交进链的内容）的**唯一权威**是 provider
侧的流累积状态（StreamAccumulator），由 ReActLoop 持有句柄（turn 级
`_current_acc`）。对外两个覆盖互斥的投影：

- `snapshot_blocks(acc)`：**已终结**块（text/thinking/已终结 tool_use）——
  用于中断补提交与未提交 Message 投影；
- `pending_tool_calls(acc)`：**未终结** tool 调用的原始 args 文本——用于活
  工具卡渲染（后端不解析半截 JSON）。

`WingAgent.uncommitted_message()` / `uncommitted_tools()` 按需快照当前一轮的
accumulator（不缓存副本）；轮提交 / 中断补提交 / turn 收口后 `_current_acc`
置空，投影失效——多轮 turn 不双重呈现。
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from wing.event import DoneEvent
from wing.event_bus import event_bus
from wing.provider.anthropic import AnthropicProvider, _StreamState
from wing.provider.openai_compat import OpenAICompatProvider, _OAIStreamState
from wing.provider.base import PendingToolView, StreamAccumulator
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    PendingCall,
    TextBlock,
    ThinkingBlock,
    ToolCall,
    ToolUseBlock,
)


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


def _start_turn(agent, content: str = "go") -> None:
    from wing.agent.inbox import Inbound

    agent._inbox._queue.put_nowait(
        Inbound(message=Message(role="user", content=content))
    )


# ============================================================
# Provider 级：双投影覆盖互斥
# ============================================================


def _anthropic_state() -> _StreamState:
    """thinking + text + 已终结 tool（index 2）+ 半截 tool（index 3, pending）。"""
    state = _StreamState()
    state.blocks_by_index[0] = ThinkingBlock(thinking="thought", signature="s")
    state.blocks_by_index[1] = TextBlock(text="answer")
    state.blocks_by_index[2] = ToolUseBlock(id="done-call", name="Edit", input={"p": 1})
    state.blocks_by_index[3] = ToolUseBlock(id="half-call", name="Read", input={})
    # index 3 未收到 content_block_stop —— 仍 pending，携带半截原始 args
    state.pending_tools[3] = PendingCall(
        id="half-call", name="Read", args_buffer='{"path": "mai'
    )
    return state


class TestAnthropicDoubleProjection:
    def _provider(self) -> AnthropicProvider:
        return AnthropicProvider.__new__(AnthropicProvider)  # 不走 __init__（无 http）

    def test_finalized_and_pending_mutually_exclusive(self):
        """已终结块进 snapshot_blocks，未终结调用进 pending_tool_calls，互斥。"""
        provider = self._provider()
        acc = provider.create_accumulator()
        acc.state = _anthropic_state()

        blocks = provider.snapshot_blocks(acc)
        assert blocks is not None
        block_ids = {getattr(b, "id", None) for b in blocks}
        assert "done-call" in block_ids  # 已终结 tool 进 snapshot
        assert "half-call" not in block_ids  # 未终结 tool 不进 snapshot

        pending = provider.pending_tool_calls(acc)
        pending_ids = {v.tool_call_id for v in pending}
        assert pending_ids == {"half-call"}  # 未终结 tool 进 pending
        assert "done-call" not in pending_ids  # 已终结 tool 不进 pending

    def test_pending_carries_raw_args_no_parsing(self):
        """pending 投影只搬运原始 args 文本，后端不解析半截 JSON。"""
        provider = self._provider()
        acc = provider.create_accumulator()
        acc.state = _anthropic_state()

        (view,) = provider.pending_tool_calls(acc)
        assert isinstance(view, PendingToolView)
        assert view.tool_call_id == "half-call"
        assert view.tool_name == "Read"
        # 原始半截文本原样搬运（不是解析后的 dict）
        assert view.args_fragment == '{"path": "mai'

    def test_both_empty_when_stream_never_started(self):
        """流未开始（state 未填充）：snapshot None、pending 空。"""
        provider = self._provider()
        acc = provider.create_accumulator()
        assert provider.snapshot_blocks(acc) is None
        assert provider.pending_tool_calls(acc) == []
        assert provider.pending_tool_calls(None) == []

    def test_pending_skips_id_less_call(self):
        """尚无 id 的半截调用无法锚定，跳过。"""
        provider = self._provider()
        state = _StreamState()
        state.blocks_by_index[0] = ToolUseBlock(id="", name="Read", input={})
        state.pending_tools[0] = PendingCall(id="", name="Read", args_buffer='{"x"')
        acc = provider.create_accumulator()
        acc.state = state
        assert provider.pending_tool_calls(acc) == []


def _oai_state() -> _OAIStreamState:
    """reasoning + content + 已终结 tool（final_tool_calls）+ 半截 tool（pending）。"""
    state = _OAIStreamState()
    state.reasoning_chunks.append("think ")
    state.reasoning_chunks.append("more")
    state.content_chunks.append("answer")
    state.final_tool_calls.append(ToolCall(id="c1", name="Edit", arguments={"p": 1}))
    state.pending[1] = PendingCall(id="c2", name="Bash", args_buffer='{"comma')
    return state


class TestOpenAIDoubleProjection:
    def _provider(self) -> OpenAICompatProvider:
        return OpenAICompatProvider.__new__(OpenAICompatProvider)

    def test_finalized_and_pending_mutually_exclusive(self):
        provider = self._provider()
        acc = provider.create_accumulator()
        acc.state = _oai_state()

        blocks = provider.snapshot_blocks(acc)
        assert blocks is not None
        block_ids = {getattr(b, "id", None) for b in blocks}
        assert "c1" in block_ids  # final_tool_calls 进 snapshot
        assert "c2" not in block_ids  # pending 不进 snapshot

        pending = provider.pending_tool_calls(acc)
        assert {v.tool_call_id for v in pending} == {"c2"}

    def test_pending_carries_raw_args(self):
        provider = self._provider()
        acc = provider.create_accumulator()
        acc.state = _oai_state()
        (view,) = provider.pending_tool_calls(acc)
        assert view.tool_call_id == "c2"
        assert view.tool_name == "Bash"
        assert view.args_fragment == '{"comma'

    def test_both_empty_when_never_started(self):
        provider = self._provider()
        acc = provider.create_accumulator()
        assert provider.snapshot_blocks(acc) is None
        assert provider.pending_tool_calls(acc) == []
        acc.state = _OAIStreamState()  # 流已开始但无内容
        assert provider.snapshot_blocks(acc) is None
        assert provider.pending_tool_calls(acc) == []


# ============================================================
# Agent 级：未提交投影按需快照（不缓存副本）
# ============================================================


def _oai_acc(
    *,
    text: str | None = None,
    finalized_tools: list[ToolCall] | None = None,
    pending: dict[int, PendingCall] | None = None,
) -> StreamAccumulator:
    """构造一个携带 _OAIStreamState 的 accumulator（默认 provider 为 openai）。"""
    state = _OAIStreamState()
    if text:
        state.content_chunks.append(text)
    for tc in finalized_tools or []:
        state.final_tool_calls.append(tc)
    for idx, pc in (pending or {}).items():
        state.pending[idx] = pc
    acc = StreamAccumulator()
    acc.state = state
    return acc


class TestAgentUncommittedProjection:
    @pytest.mark.asyncio
    async def test_idle_agent_projection_empty(self, runtime):
        """无进行中轮次：未提交投影为 null / 空。"""
        session = runtime.create_session()
        agent = session.agent
        assert agent._loop.current_acc is None
        assert agent.uncommitted_message() is None
        assert agent.uncommitted_tools() == []
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_uncommitted_message_projects_finalized_tool_use(self, runtime):
        """未提交 Message 投影**含已终结 tool_use 块**——diff 锚定的结构前提。"""
        session = runtime.create_session()
        agent = session.agent
        agent._loop._current_acc = _oai_acc(
            text="let me edit",
            finalized_tools=[
                ToolCall(id="tc-edit", name="Edit", arguments={"path": "f"})
            ],
        )

        proj = agent.uncommitted_message()
        assert proj is not None
        assert proj["role"] == "assistant"
        assert proj["content"] == "let me edit"
        assert proj["tool_calls"] == [
            {"id": "tc-edit", "name": "Edit", "arguments": {"path": "f"}}
        ]
        # 无未终结调用
        assert agent.uncommitted_tools() == []
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_uncommitted_tools_projects_pending_raw_args(self, runtime):
        """未终结调用进 uncommitted_tools（原始 args），且不进 Message 投影。"""
        session = runtime.create_session()
        agent = session.agent
        agent._loop._current_acc = _oai_acc(
            pending={
                0: PendingCall(id="tc-bash", name="Bash", args_buffer='{"command": "sl')
            }
        )

        tools = agent.uncommitted_tools()
        assert tools == [
            {
                "tool_call_id": "tc-bash",
                "tool_name": "Bash",
                "args_fragment": '{"command": "sl',
            }
        ]
        # 只有半截调用（无已终结块）→ Message 投影为 null（互斥）
        assert agent.uncommitted_message() is None
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_projection_is_on_demand_not_cached(self, runtime):
        """投影按需快照：accumulator 状态变化后，下一次投影立即反映（无缓存副本）。"""
        session = runtime.create_session()
        agent = session.agent
        # 持有 typed state 引用，模拟流持续累积进同一 accumulator
        state = _OAIStreamState()
        state.content_chunks.append("first")
        acc = StreamAccumulator()
        acc.state = state
        agent._loop._current_acc = acc
        assert agent.uncommitted_message()["content"] == "first"

        # 流继续累积（同一 accumulator 对象）→ 投影立即反映，无缓存副本
        state.content_chunks.append(" more")
        assert agent.uncommitted_message()["content"] == "first more"
        await agent.shutdown()

    @pytest.mark.asyncio
    async def test_interrupt_and_resume_same_source(self, runtime):
        """6.11：中断补提交与 resume 未提交投影**同源**（同一 snapshot_blocks）。

        同一 accumulator 状态下，中断将提交的块数组组装出的 Message 投影，
        与 resume 下发的 uncommitted 投影逐字段相等——不再是两条必须互相
        保持一致的代码路径。
        """
        from wing.session import serialize_message

        session = runtime.create_session()
        agent = session.agent
        acc = _oai_acc(
            text="answer",
            finalized_tools=[ToolCall(id="tc1", name="Edit", arguments={"path": "f"})],
        )
        agent._loop._current_acc = acc

        # 中断路径将用 snapshot_blocks 组装 partial Message
        committed_blocks = agent.model_provider.snapshot_blocks(acc)
        interrupt_projection = serialize_message(
            Message(role="assistant", content_blocks=committed_blocks)
        )
        # resume 路径的未提交投影
        resume_projection = agent.uncommitted_message()

        assert resume_projection == interrupt_projection
        assert resume_projection["tool_calls"][0]["id"] == "tc1"
        await agent.shutdown()


# ============================================================
# 轮边界：投影失效（防多轮双重呈现）
# ============================================================


class TestTurnBoundaryInvalidation:
    @pytest.mark.asyncio
    async def test_projection_invalidated_after_turn_commit(self, runtime, monkeypatch):
        """一轮提交进链后 `_current_acc` 置空——内容只在 messages 呈现一次。"""
        session = runtime.create_session()
        agent = session.agent

        done = asyncio.Event()
        event_bus.subscribe(lambda e: done.set() if isinstance(e, DoneEvent) else None)

        async def fake_generate(*args: Any, **kwargs: Any):
            acc = kwargs.get("accumulator")
            state = _OAIStreamState()
            state.content_chunks.append("round answer")
            if acc is not None:
                acc.state = state
            yield LLMResponse(
                content_blocks=[TextBlock(text="round answer")],
                usage=LLMUsage(stop_reason="stop"),
            )

        monkeypatch.setattr(agent.model_provider, "generate", fake_generate)

        _start_turn(agent)
        await asyncio.wait_for(done.wait(), timeout=5)

        # turn 收口：accumulator 置空，未提交投影失效
        assert agent._loop.current_acc is None
        assert agent.uncommitted_message() is None
        assert agent.uncommitted_tools() == []

        # 内容已提交进 messages，且只呈现一次（无双重渲染）
        chain = agent.context_manager.get_context_window()
        assert [m.role for m in chain] == ["user", "assistant"]
        assert chain[1].content == "round answer"
        await agent.shutdown()
