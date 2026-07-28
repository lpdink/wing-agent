# Tests for compaction refactor -- TDD.
#
# Core changes:
# 1. Compactor no longer holds model/model_provider
# 2. do_compact accepts full_messages + model + model_provider
# 3. COMPACT_PROMPT appended as last user message (cache-friendly)
# 4. tool_calls in compact response silently ignored
# 5. _calc_cut_idx extracted as reusable helper
# 6. get_messages_for_llm receives model + model_provider
# 7. AgentConfig tolerates old compact_model field

from __future__ import annotations

import shutil
import tempfile
from pathlib import Path
from typing import AsyncIterator

import pytest

from wing.common.tracked_list import TrackedList
from wing.store import FileMessageLog
from wing.compactor import Compactor
from wing.context_manager import ContextManager
from wing.schema import LLMResponse, Message, ToolCall


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp()
    yield Path(d)
    shutil.rmtree(d)


def _make_cm(tmp_dir: Path, compactor: Compactor | None = None) -> ContextManager:
    sid = "test-session"
    messages: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir / sid))
    if compactor is None:
        compactor = Compactor(
            context_window_tokens=100_000,
            keep_recent_tokens=20_000,
        )
    return ContextManager(
        session_id=sid,
        messages=messages,
        system_prompt="You are a helpful assistant.",
        compactor=compactor,
    )


def _mock_provider(response: LLMResponse):
    """Return a provider whose .generate() yields a single response."""

    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        yield response

    return type("MockProvider", (), {"generate": _gen})()


def _capturing_provider():
    """Return (provider, state_dict) -- state_dict is populated on call."""
    state: dict = {}

    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        state["messages"] = messages
        state["model"] = model
        state["tools"] = tools
        yield LLMResponse(content="<summary>ok</summary>")

    return type("MockProvider", (), {"generate": _gen})(), state


DEFAULT_MODEL = "test-model"


# ===================================================================
# 1. Compactor Init
# ===================================================================


class TestCompactorInit:
    def test_init_without_model_or_provider(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        assert h.context_window_tokens == 100_000
        assert h.compact_window_tokens == 80_000

    def test_init_custom_tokens(self):
        h = Compactor(context_window_tokens=50_000, keep_recent_tokens=10_000)
        assert h.context_window_tokens == 50_000
        assert h.compact_window_tokens == 40_000

    def test_no_model_attr(self):
        assert not hasattr(
            Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000), "model"
        )
        assert not hasattr(
            Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000),
            "model_provider",
        )


# ===================================================================
# 2. do_compact
# ===================================================================


class TestDoCompact:
    @pytest.mark.asyncio
    async def test_appends_compact_prompt_as_last_user_message(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [
            Message(role="system", content="You are helpful."),
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        prov, state = _capturing_provider()
        resp = await h.do_compact(full, DEFAULT_MODEL, prov)
        # verify last message is COMPACT_PROMPT
        msgs = state["messages"]
        assert len(msgs) == len(full) + 1
        assert msgs[-1].role == "user"
        assert "Write a continuation summary" in (msgs[-1].content or "")
        assert "DO NOT call any tools" in (msgs[-1].content or "")
        assert resp.content == "[Compact] ok"
        # verify system prompt and history preserved
        assert msgs[0].content == "You are helpful."
        assert msgs[1].content == "hello"
        assert msgs[2].content == "hi"

    @pytest.mark.asyncio
    async def test_tool_calls_silently_ignored(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov = _mock_provider(
            LLMResponse(
                content="prefix<summary>real summary</summary>suffix",
                tool_calls=[ToolCall(id="x", name="Bash", arguments={"x": "y"})],
            )
        )
        resp = await h.do_compact(full, DEFAULT_MODEL, prov)
        assert resp.content == "[Compact] real summary"

    @pytest.mark.asyncio
    async def test_reasoning_content_preserved(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov = _mock_provider(
            LLMResponse(content="<summary>x</summary>", reasoning_content="think...")
        )
        resp = await h.do_compact(full, DEFAULT_MODEL, prov)
        assert resp.reasoning_content == "think..."

    @pytest.mark.asyncio
    async def test_missing_summary_tags_raises(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov = _mock_provider(LLMResponse(content="no summary tags"))
        with pytest.raises(RuntimeError, match="extract summary"):
            await h.do_compact(full, DEFAULT_MODEL, prov)

    @pytest.mark.asyncio
    async def test_model_passed_through(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov, state = _capturing_provider()
        await h.do_compact(full, "my-main-model", prov)
        assert state["model"] == "my-main-model"

    @pytest.mark.asyncio
    async def test_tools_passed_through(self):
        """tools 参数被透传给 provider.generate。"""
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov, state = _capturing_provider()
        my_tools = ["Bash", "Read"]
        await h.do_compact(full, "m", prov, tools=my_tools)
        assert state["tools"] == my_tools

    @pytest.mark.asyncio
    async def test_tools_defaults_to_none(self):
        """不传 tools 时透传 None（与主 agent 调用不一致时不命中缓存，但兼容）。"""
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov, state = _capturing_provider()
        await h.do_compact(full, "m", prov)
        assert state["tools"] is None

    @pytest.mark.asyncio
    async def test_empty_full_messages(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        prov, state = _capturing_provider()
        await h.do_compact([], DEFAULT_MODEL, prov)
        assert len(state["messages"]) == 1
        assert state["messages"][0].role == "user"


# ===================================================================
# 3. _calc_cut_idx
# ===================================================================


class TestCalcCutIdx:
    def test_basic_cut(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=80_000)
        # compact_window_tokens = 20_000
        msgs = [
            Message(role="user", content="short"),
            Message(
                role="assistant", content="x" * 80_000
            ),  # ~20K tokens, exceeds window
            Message(role="user", content="tail"),
        ]
        idx = h._calc_cut_idx(msgs)
        assert idx == 2  # first two messages compacted

    def test_cut_all_if_small(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [Message(role="user", content="tiny") for _ in range(10)]
        idx = h._calc_cut_idx(msgs)
        assert idx == len(msgs)

    def test_tool_call_integrity(self):
        # A tool call at msg[1] must be paired with its tool result at msg[2]
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=99_000)
        msgs = [
            Message(role="user", content="x" * 100),
            Message(
                role="assistant",
                content="",
                tool_calls=[ToolCall(id="tc1", name="Bash", arguments={"x": "y"})],
            ),
            Message(role="tool", content="result", tool_call_id="tc1"),
            Message(role="user", content="tail"),
        ]
        idx = h._calc_cut_idx(msgs)
        # All messages should be cut because window is large enough
        assert idx == len(msgs)

    def test_tool_call_integrity_with_pending(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [
            Message(role="user", content="short"),
            Message(
                role="assistant",
                content="",
                tool_calls=[ToolCall(id="tc1", name="Bash", arguments={"x": "y"})],
            ),
            Message(role="tool", content="result", tool_call_id="tc1"),
            Message(role="user", content="tail"),
        ]
        idx = h._calc_cut_idx(msgs)
        # Should include the tool response pair
        assert idx >= 3

    def test_empty_list(self):
        assert (
            Compactor(
                context_window_tokens=100_000, keep_recent_tokens=20_000
            )._calc_cut_idx([])
            == 0
        )


# ===================================================================
# 4. get_messages_for_llm -- new signature
# ===================================================================


class TestGetMessagesForLlm:
    @pytest.mark.asyncio
    async def test_accepts_model_and_provider(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        cm.add_message(Message(role="user", content="hello"))
        llm_msgs = (
            await cm.get_messages_for_llm(
                model=DEFAULT_MODEL,
                model_provider=_mock_provider(
                    LLMResponse(content="<summary>t</summary>")
                ),
                current_tools=lambda: [],
            )
        ).messages
        assert len(llm_msgs) == 2
        assert llm_msgs[1].content == "hello"

    @pytest.mark.asyncio
    async def test_no_compact_path(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        cm.add_message(Message(role="user", content="hello"))
        cm.add_message(Message(role="assistant", content="hi"))
        llm_msgs = (
            await cm.get_messages_for_llm(
                model=DEFAULT_MODEL,
                model_provider=_mock_provider(
                    LLMResponse(content="<summary>t</summary>")
                ),
                current_tools=lambda: [],
            )
        ).messages
        assert len(llm_msgs) == 3

    @pytest.mark.asyncio
    async def test_compact_uses_main_model(self, tmp_dir):
        """Background compact passes the main model to the provider."""
        # Use very low trigger threshold to force early compact
        low_threshold = Compactor(context_window_tokens=50, keep_recent_tokens=10)
        cm = _make_cm(tmp_dir, compactor=low_threshold)
        cm.add_message(Message(role="user", content="x" * 200))
        cm.add_message(Message(role="assistant", content="y" * 200))
        cm.add_message(Message(role="user", content="z" * 50))
        prov, state = _capturing_provider()
        await cm.get_messages_for_llm(
            model="main-model", model_provider=prov, current_tools=lambda: []
        )
        # Wait for background task
        if cm._pending_compact_task:
            await cm._pending_compact_task
        assert state.get("model") == "main-model"

    @pytest.mark.asyncio
    async def test_compact_failure_no_result(self, tmp_dir):
        """Background compact failure → result stays None, original messages returned."""
        low_threshold = Compactor(context_window_tokens=50, keep_recent_tokens=10)
        cm = _make_cm(tmp_dir, compactor=low_threshold)
        cm.add_messages(
            [
                Message(role="user", content="x" * 500),
                Message(role="assistant", content="y" * 500),
                Message(role="user", content="z" * 500),
            ]
        )
        prov = _mock_provider(LLMResponse(content="no summary tags"))
        llm_msgs = (
            await cm.get_messages_for_llm(
                model="m", model_provider=prov, current_tools=lambda: []
            )
        ).messages
        # Wait for background task to fail
        if cm._pending_compact_task:
            await cm._pending_compact_task
        # No result set, original messages returned
        assert cm._pending_compact_result is None
        assert len(llm_msgs) == 4  # system + 3 original messages


# ===================================================================
# 6. Integration -- compact then continue
# ===================================================================


class TestCompactIntegration:
    @pytest.mark.asyncio
    async def test_compact_then_continue(self, tmp_dir):
        """Full async compact flow: early trigger → background → apply → continue."""
        low_threshold = Compactor(context_window_tokens=50, keep_recent_tokens=10)
        cm = _make_cm(tmp_dir, compactor=low_threshold)
        cm.add_messages(
            [
                Message(role="user", content="old1"),
                Message(role="assistant", content="old2 is a long message " * 8),
                Message(role="user", content="old3"),
            ]
        )

        async def _gen(
            self,
            messages: list[Message],
            model: str,
            tools: list | None = None,
            stream: bool = False,
        ) -> AsyncIterator[LLMResponse]:
            yield LLMResponse(content="<summary>compressed old1 and old2</summary>")

        prov = type("MockProvider", (), {"generate": _gen})()

        # First call: should trigger early compact (background)
        await cm.get_messages_for_llm(
            model="main-model",
            model_provider=prov,  # ty: ignore[invalid-argument-type]
            current_tools=lambda: [],
        )

        # Wait for background task
        if cm._pending_compact_task:
            await cm._pending_compact_task

        # Add more messages to reach apply threshold
        cm.add_message(Message(role="assistant", content="resp " * 20))
        cm.add_message(Message(role="user", content="more " * 20))

        # Second call: should apply the pending compact
        llm_msgs = (
            await cm.get_messages_for_llm(
                model="main-model",
                model_provider=prov,  # ty: ignore[invalid-argument-type]
                current_tools=lambda: [],
            )
        ).messages

        assert len(llm_msgs) >= 2
        # compact node should be in the active chain (if apply happened)
        compact_found = any("[Compact]" in (m.content or "") for m in llm_msgs)
        # Either applied or still pending
        if not compact_found:
            assert cm._pending_compact_result is not None  # still pending

        # continue chatting after compact
        cm.add_message(Message(role="user", content="new question"))
        window = cm.get_context_window()
        assert len(window) >= 2
        assert "new question" in (window[-1].content or "")
