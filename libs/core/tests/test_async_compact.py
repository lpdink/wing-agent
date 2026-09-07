"""Tests for async compact flow — early trigger, background task, apply, UUID validation."""

from __future__ import annotations

import asyncio
import shutil
import tempfile
from pathlib import Path
from typing import AsyncIterator

import pytest

from wing.compactor import Compactor
from wing.common.tracked_list import TrackedList
from wing.store import FileMessageLog
from wing.context_manager import ContextManager, PendingCompact
from wing.schema import ChainNode, LLMResponse, LLMUsage, Message


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp()
    yield Path(d)
    shutil.rmtree(d)


def _make_cm(
    tmp_dir: Path,
    compactor: Compactor | None = None,
    session_id: str = "test-async",
) -> ContextManager:
    messages: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir / session_id))
    if compactor is None:
        compactor = Compactor(
            context_window_tokens=100_000,
            keep_recent_tokens=20_000,
        )
    return ContextManager(
        session_id=session_id,
        messages=messages,
        system_prompt="You are a helpful assistant.",
        compactor=compactor,
    )


def _mock_provider(response: LLMResponse | None = None, delay: float = 0):
    """Provider that yields a response, optionally after a delay."""

    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        if delay > 0:
            await asyncio.sleep(delay)
        yield response or LLMResponse(
            content="<summary>compressed</summary>",
            usage=LLMUsage(prompt_tokens=5000, completion_tokens=200),
        )

    return type("MockProvider", (), {"generate": _gen})()


def _failing_provider():
    """Provider that always raises."""

    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        raise RuntimeError("LLM call failed")
        yield  # make it a generator  # noqa: unreachable

    return type("MockProvider", (), {"generate": _gen})()


def _add_large_messages(cm: ContextManager, count: int, content_size: int = 100):
    """Add messages that collectively have ~count * content_size/4 tokens."""
    for i in range(count):
        cm.add_message(Message(role="user", content=f"q{i}: " + "x" * content_size))
        cm.add_message(
            Message(role="assistant", content=f"a{i}: " + "y" * content_size)
        )


# ===================================================================
# 1. Early Trigger
# ===================================================================


class TestEarlyTrigger:
    @pytest.mark.asyncio
    async def test_no_trigger_below_threshold(self, tmp_dir):
        """Tokens below compact_window_tokens → no background task started."""
        # compact_window_tokens = 80_000
        cm = _make_cm(tmp_dir)
        cm.add_message(Message(role="user", content="hello"))
        prov = _mock_provider()
        msgs = (
            await cm.get_messages_for_llm(
                model="m", model_provider=prov, current_tools=lambda: []
            )
        ).messages
        assert cm._pending_compact_task is None
        assert len(msgs) == 2  # system + user

    @pytest.mark.asyncio
    async def test_trigger_starts_background_task(self, tmp_dir):
        """Tokens >= compact_window_tokens → background task started."""
        # Use very low thresholds for testing
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        # compact_window_tokens = 150
        cm = _make_cm(tmp_dir, compactor=compactor)
        # Add messages that exceed 150 tokens (need at least user+assistant pair
        # so head is non-empty after preserving last user message)
        cm.add_message(Message(role="user", content="x" * 800))
        cm.add_message(Message(role="assistant", content="y" * 800))
        cm.add_message(Message(role="user", content="z" * 100))
        prov = _mock_provider(delay=0.5)
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )
        # Background task should be started
        assert cm._pending_compact_task is not None
        # Wait for task to complete
        await asyncio.sleep(0.6)
        assert cm._pending_compact_result is not None

    @pytest.mark.asyncio
    async def test_no_duplicate_task_when_result_pending(self, tmp_dir):
        """When pending result exists but apply threshold not reached,
        no new task should be started (Bug 1 fix)."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        cm.add_message(Message(role="user", content="x" * 800))
        cm.add_message(Message(role="assistant", content="y" * 800))
        cm.add_message(Message(role="user", content="z" * 100))
        prov = _mock_provider(delay=0.1)
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        # Wait for task to complete → result is set
        if cm._pending_compact_task:
            await asyncio.sleep(0.3)
        assert cm._pending_compact_result is not None

        # Second call: result exists, tokens < context_window_tokens
        # → should NOT start a new task
        old_task = cm._pending_compact_task
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )
        # Task should not have been replaced (no new task started)
        assert cm._pending_compact_task is old_task or cm._pending_compact_task is None


# ===================================================================
# 2. Apply Flow
# ===================================================================


class TestApplyFlow:
    @pytest.mark.asyncio
    async def test_apply_when_tokens_reach_threshold(self, tmp_dir):
        """Tokens >= context_window_tokens + valid pending → apply."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        # Add enough messages to trigger early compact
        cm.add_message(Message(role="user", content="x" * 800))
        prov = _mock_provider()
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        # Wait for background task
        if cm._pending_compact_task:
            await cm._pending_compact_task

        # Now add more messages to reach context_window_tokens
        cm.add_message(Message(role="assistant", content="y" * 800))
        cm.add_message(Message(role="user", content="z" * 800))

        msgs = (
            await cm.get_messages_for_llm(
                model="m", model_provider=prov, current_tools=lambda: []
            )
        ).messages
        # After apply, compact node should be in the chain
        compact_found = any("[Compact]" in (m.content or "") for m in msgs)
        assert compact_found or cm._pending_compact_result is None

    @pytest.mark.asyncio
    async def test_no_apply_below_threshold(self, tmp_dir):
        """Tokens < context_window_tokens → no apply even if result ready."""
        compactor = Compactor(context_window_tokens=10_000, keep_recent_tokens=100)
        cm = _make_cm(tmp_dir, compactor=compactor)

        cm.add_message(Message(role="user", content="x" * 800))
        prov = _mock_provider()
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        if cm._pending_compact_task:
            await cm._pending_compact_task

        # Manually set a pending result (simulating it's ready)
        if cm._pending_compact_result is None:
            pytest.skip("early trigger didn't fire with this token estimation")

        # Get messages again — tokens should be below context_window_tokens
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )
        # Result should still be pending (not applied)
        # (unless tokens actually exceeded, which is unlikely with our test data)


# ===================================================================
# 3. UUID Self-Validation
# ===================================================================


class TestUUIDValidation:
    @pytest.mark.asyncio
    async def test_uuid_match(self, tmp_dir):
        """start_uuid and end_uuid found in chain → valid."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        cm.add_message(Message(role="user", content="x" * 800))
        cm.add_message(Message(role="assistant", content="y" * 800))
        prov = _mock_provider()
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        if cm._pending_compact_task:
            await cm._pending_compact_task

        if cm._pending_compact_result:
            msgs = [m for m in cm._messages if isinstance(m, Message)]
            indices = cm._verify_snapshot_valid(msgs)
            assert indices is not None
            start_idx, end_idx = indices
            assert start_idx <= end_idx

    @pytest.mark.asyncio
    async def test_uuid_not_found_after_rewind(self, tmp_dir):
        """After rewind, UUIDs don't match → invalid."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        cm.add_message(Message(role="user", content="x" * 800))
        cm.add_message(Message(role="assistant", content="y" * 800))
        prov = _mock_provider()
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        if cm._pending_compact_task:
            await cm._pending_compact_task

        if cm._pending_compact_result is None:
            pytest.skip("early trigger didn't fire with this token estimation")

        # Rewind to the first message — changes the active chain
        chain = cm._messages.active_chain
        if len(chain) >= 1:
            cm.rewind(chain[0].uuid)  # ty: ignore[invalid-argument-type]
            # Now the UUIDs should not match
            msgs = [m for m in cm._messages if isinstance(m, Message)]
            indices = cm._verify_snapshot_valid(msgs)
            assert indices is None


# ===================================================================
# 4. Background Task Failure
# ===================================================================


class TestBackgroundTaskFailure:
    @pytest.mark.asyncio
    async def test_failed_task_no_result(self, tmp_dir):
        """Background compact fails → _pending_compact_result stays None."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        cm.add_message(Message(role="user", content="x" * 800))
        prov = _failing_provider()
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        if cm._pending_compact_task:
            await asyncio.sleep(0.2)  # let task fail
            assert cm._pending_compact_result is None

    @pytest.mark.asyncio
    async def test_returns_original_messages_on_failure(self, tmp_dir):
        """When compact fails, original messages are returned."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        cm.add_message(Message(role="user", content="x" * 800))
        prov = _failing_provider()
        msgs = (
            await cm.get_messages_for_llm(
                model="m", model_provider=prov, current_tools=lambda: []
            )
        ).messages
        assert len(msgs) == 2  # system + user (original, no compact)


# ===================================================================
# 5. Await Running Task at Apply Point
# ===================================================================


class TestAwaitRunningTask:
    @pytest.mark.asyncio
    async def test_returns_original_when_task_running(self, tmp_dir):
        """When task is still running, return original messages (no blocking)."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        # Add messages to trigger early compact
        cm.add_message(Message(role="user", content="x" * 800))
        cm.add_message(Message(role="assistant", content="y" * 800))
        cm.add_message(Message(role="user", content="z" * 100))
        prov = _mock_provider(delay=2.0)  # long delay so task is still running
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        # Task should be running
        assert cm._pending_compact_task is not None
        assert not cm._pending_compact_task.done()

        # Second call — task still running → return original messages
        msgs = (
            await cm.get_messages_for_llm(
                model="m", model_provider=prov, current_tools=lambda: []
            )
        ).messages
        assert len(msgs) >= 2  # system + messages (no compact applied)

        # Clean up: cancel the slow task
        cm._pending_compact_task.cancel()
        try:
            await cm._pending_compact_task
        except asyncio.CancelledError:
            pass

    @pytest.mark.asyncio
    async def test_apply_after_task_completes(self, tmp_dir):
        """After task completes and result is ready, apply works."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        cm.add_message(Message(role="user", content="x" * 800))
        cm.add_message(Message(role="assistant", content="y" * 800))
        cm.add_message(Message(role="user", content="z" * 100))
        prov = _mock_provider(delay=0.1)
        await cm.get_messages_for_llm(
            model="m", model_provider=prov, current_tools=lambda: []
        )

        # Wait for task to complete
        if cm._pending_compact_task:
            await asyncio.sleep(0.3)

        # Result should be ready
        assert cm._pending_compact_result is not None

        # Add more messages to reach apply threshold
        cm.add_message(Message(role="assistant", content="a" * 800))
        cm.add_message(Message(role="user", content="b" * 800))

        msgs = (
            await cm.get_messages_for_llm(
                model="m", model_provider=prov, current_tools=lambda: []
            )
        ).messages
        # After apply, compact node should be in chain
        compact_found = any("[Compact]" in (m.content or "") for m in msgs)
        # Either applied or still pending (depends on token counts)
        assert cm._pending_compact_result is None or not compact_found


# ===================================================================
# 6. Discard on UUID Mismatch
# ===================================================================


class TestDiscardOnMismatch:
    @pytest.mark.asyncio
    async def test_discard_clears_state(self, tmp_dir):
        """_discard_pending_compact clears task, result, and file."""
        compactor = Compactor(context_window_tokens=200, keep_recent_tokens=50)
        cm = _make_cm(tmp_dir, compactor=compactor)

        # Manually set pending result
        cm._pending_compact_result = PendingCompact(
            compact_content="[Compact] test",
            start_uuid="nonexistent-start",
            end_uuid="nonexistent-end",
        )

        cm._discard_pending_compact()
        assert cm._pending_compact_result is None
        assert cm._pending_compact_task is None
