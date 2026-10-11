"""手动压缩（/compact）与 pending compact 状态机的直击测试。

`do_manual_compact` 是用户显式下发的压缩：丢弃后台预计算结果、同步压缩、
把摘要节点换入链尾。这条路径此前完全没有测试——补上它，连同两处
「调用方用错」的 RuntimeError 与 pending 的持久化失败降级。
"""

from __future__ import annotations

import asyncio
import json
import shutil
import tempfile
from pathlib import Path
from typing import Any, AsyncIterator

import pytest

from wing.chain import TrackedList
from wing.context import Compactor, ContextManager, PendingCompact
from wing.schema import ChainNode, LLMResponse, LLMUsage, Message
from wing.store.file import FileMessageLog

pytestmark = pytest.mark.asyncio

_SUMMARY = "[Compact] compressed"
_DEFAULT_COMPACTOR = object()


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp()
    yield Path(d)
    shutil.rmtree(d)


def _mock_provider(summary: str = "<summary>compressed</summary>"):
    calls: list[dict] = []

    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
        options: object = None,
    ) -> AsyncIterator[LLMResponse]:
        calls.append({"messages": messages, "model": model, "options": options})
        yield LLMResponse(
            content=summary, usage=LLMUsage(prompt_tokens=4321, completion_tokens=123)
        )

    provider = type("MockProvider", (), {"generate": _gen})()
    return provider, calls


def _make_cm(
    tmp_dir: Path,
    compactor: Any = _DEFAULT_COMPACTOR,
    session_id: str = "test-manual",
) -> ContextManager:
    messages: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir / session_id))
    if compactor is _DEFAULT_COMPACTOR:
        compactor = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
    return ContextManager(
        session_id=session_id,
        messages=messages,
        system_prompt="You are a helpful assistant.",
        compactor=compactor,
    )


def _seed(cm: ContextManager) -> None:
    cm.add_message(Message(role="user", content="first question"))
    cm.add_message(Message(role="assistant", content_blocks=[]))


# ── do_manual_compact ─────────────────────────────────────────


class TestManualCompact:
    async def test_appends_summary_node_and_returns_usage(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        _seed(cm)
        before = cm.get_context_window()
        provider, calls = _mock_provider()
        synced: list[str] = []

        tokens = await cm.do_manual_compact(
            model="m",
            model_provider=provider,
            current_tools=lambda: synced.append("called") or [],
            instruction="keep decisions",
        )

        assert tokens == (4321, 123)
        window = cm.get_context_window()
        # 摘要节点成为新的活跃尾节点，并编码被压缩区间的末消息
        assert len(window) == 1
        summary_node = window[0]
        assert isinstance(summary_node, Message)
        assert summary_node.content == _SUMMARY
        assert summary_node.unzip_last_uuid == before[-1].uuid
        assert cm.get_context_window()[-1].uuid == summary_node.uuid
        # Compact 打破 prefix cache → 声明集在 await 后重新同步
        assert synced == ["called"]
        # 指令透传给 Compactor（渲染进压缩 prompt）
        assert "keep decisions" in calls[0]["messages"][-1].content

    async def test_without_compactor_raises(self, tmp_dir):
        cm = _make_cm(tmp_dir, compactor=None)
        provider, _ = _mock_provider()

        with pytest.raises(RuntimeError, match="compactor not configured"):
            await cm.do_manual_compact(
                model="m", model_provider=provider, current_tools=lambda: []
            )

    async def test_empty_chain_compacts_system_prompt_only(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        provider, _ = _mock_provider()

        await cm.do_manual_compact(
            model="m", model_provider=provider, current_tools=lambda: []
        )

        window = cm.get_context_window()
        assert len(window) == 1
        assert window[0].unzip_last_uuid is None  # 没有可编码的区间

    async def test_discards_running_background_compact(self, tmp_dir):
        """手动压缩必须丢弃在跑的后台任务，避免结果事后覆盖手动摘要。"""
        cm = _make_cm(tmp_dir)
        _seed(cm)

        started = asyncio.Event()

        async def _slow() -> None:
            started.set()
            await asyncio.sleep(60)

        task = asyncio.create_task(_slow())
        await started.wait()
        cm._pending_compact_task = task
        cm._pending_compact_result = PendingCompact(
            compact_content="stale", start_uuid="a", end_uuid="b"
        )

        provider, _ = _mock_provider()
        await cm.do_manual_compact(
            model="m", model_provider=provider, current_tools=lambda: []
        )
        await asyncio.sleep(0)  # 让取消有机会被任务处理

        assert task.cancelled()
        assert cm._pending_compact_task is None
        assert cm._pending_compact_result is None

    async def test_history_records_summary_as_assistant_message(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        _seed(cm)
        provider, _ = _mock_provider()

        await cm.do_manual_compact(
            model="m", model_provider=provider, current_tools=lambda: []
        )

        lines = [
            json.loads(line)
            for line in (tmp_dir / "test-manual" / "history.jsonl")
            .read_text(encoding="utf-8")
            .splitlines()
            if line.strip()
        ]
        summaries = [r for r in lines if _SUMMARY in str(r)]
        assert len(summaries) == 1
        assert summaries[0]["role"] == "assistant"


# ── pending compact 状态机 ────────────────────────────────────


class TestPendingCompactState:
    async def test_no_compactor_returns_context_untouched(self, tmp_dir):
        cm = _make_cm(tmp_dir, compactor=None)
        _seed(cm)
        window = cm.get_context_window()
        provider, _ = _mock_provider()

        result = await cm.get_messages_for_llm(
            model="m", model_provider=provider, current_tools=lambda: []
        )

        assert result.messages == [cm.system_prompt] + window
        assert result.tools == []

    async def test_apply_without_pending_raises(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        with pytest.raises(RuntimeError, match="apply called without pending"):
            cm._apply_pending_compact([], 0)

    async def test_verify_snapshot_without_pending_returns_none(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        assert cm._verify_snapshot_valid([]) is None

    async def test_verify_snapshot_rejects_unknown_uuids(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        _seed(cm)
        window = cm.get_context_window()
        cm._pending_compact_result = PendingCompact(
            compact_content="x",
            start_uuid=window[0].uuid,
            end_uuid="not-in-chain",
        )
        assert cm._verify_snapshot_valid(window) is None

    async def test_verify_snapshot_requires_ordered_span(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        _seed(cm)
        window = cm.get_context_window()
        # start/end 都在链上但顺序颠倒（区间必须有序才算有效）
        cm._pending_compact_result = PendingCompact(
            compact_content="x",
            start_uuid=window[-1].uuid,
            end_uuid=window[0].uuid,
        )
        assert cm._verify_snapshot_valid(window) is None

    async def test_apply_leaves_summary_as_tip_when_nothing_to_relink(self, tmp_dir):
        """压缩区间覆盖到链尾：没有尾消息可 relink，tip 落在摘要节点上。"""
        cm = _make_cm(tmp_dir)
        _seed(cm)
        window = cm.get_context_window()
        cm._pending_compact_result = PendingCompact(
            compact_content="<summary>all</summary>",
            start_uuid=window[0].uuid,
            end_uuid=window[-1].uuid,
            usage=LLMUsage(prompt_tokens=10, completion_tokens=2),
        )

        cm._apply_pending_compact(window, len(window) - 1)

        active = cm.get_context_window()
        assert [m.content for m in active] == ["<summary>all</summary>"]
        assert cm._pending_compact_result is None
        assert cm._pending_compact_task is None

    async def test_discard_cancels_running_task(self, tmp_dir):
        cm = _make_cm(tmp_dir)

        async def _slow() -> None:
            await asyncio.sleep(60)

        task = asyncio.create_task(_slow())
        cm._pending_compact_task = task
        cm._pending_compact_result = PendingCompact(
            compact_content="x", start_uuid="a", end_uuid="b"
        )

        cm._discard_pending_compact()
        await asyncio.sleep(0)  # 让取消有机会被任务处理

        assert task.cancelled()
        assert cm._pending_compact_task is None
        assert cm._pending_compact_result is None

    async def test_persist_failure_is_swallowed(self, tmp_dir, wing_warnings):
        """aux 写入失败只降级为告警——不能把一整轮对话拖垮。"""
        cm = _make_cm(tmp_dir)

        def _boom(*args: Any, **kwargs: Any) -> None:
            raise OSError("disk full")

        cm._messages.write_aux = _boom  # type: ignore[method-assign]

        cm._persist_pending_compact(
            PendingCompact(compact_content="x", start_uuid="a", end_uuid="b")
        )

        assert any("Failed to persist pending compact" in m for m in wing_warnings)

    async def test_pending_compact_round_trips_through_aux(self, tmp_dir):
        """持久化 → 重载：start/end 与内容原样恢复（resume 后仍可 apply）。"""
        cm = _make_cm(tmp_dir)
        cm._persist_pending_compact(
            PendingCompact(
                compact_content="<summary>kept</summary>",
                start_uuid="u-1",
                end_uuid="u-2",
                usage=LLMUsage(prompt_tokens=7, completion_tokens=3),
                timestamp="2026-01-01T00:00:00",
            )
        )

        same_log: TrackedList[ChainNode] = TrackedList(
            FileMessageLog(tmp_dir / "test-manual")
        )
        restored = ContextManager(
            session_id="test-manual",
            messages=same_log,
            system_prompt="s",
            compactor=Compactor(context_window_tokens=100, keep_recent_tokens=10),
        )

        assert restored._pending_compact_result is not None
        assert (
            restored._pending_compact_result.compact_content
            == "<summary>kept</summary>"
        )
        assert restored._pending_compact_result.end_uuid == "u-2"

    async def test_corrupt_pending_payload_is_ignored(self, tmp_dir):
        """aux 里的 pending 载荷损坏（旧格式 / 手改）时按「无 pending」处理。"""
        messages: TrackedList[ChainNode] = TrackedList(
            FileMessageLog(tmp_dir / "corrupt")
        )
        messages.write_aux("pending_compact", {"unexpected": "shape"})

        cm = ContextManager(
            session_id="corrupt",
            messages=messages,
            system_prompt="s",
            compactor=Compactor(context_window_tokens=100, keep_recent_tokens=10),
        )

        assert cm._pending_compact_result is None
