"""Tests for pending compact persistence — write, read, delete, corrupt recovery.

pending compact 经由 MessageLog 的 aux kv 通道持久化（file 后端落
<session_dir>/pending_compact.json）。"""

from __future__ import annotations

import json
import shutil
import tempfile
from pathlib import Path

import pytest

from wing.compactor import Compactor
from wing.common.tracked_list import TrackedList
from wing.store import FileMessageLog
from wing.context_manager import ContextManager, PendingCompact
from wing.schema import LLMUsage, Message


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp()
    yield Path(d)
    shutil.rmtree(d)


def _aux_path(tmp_dir: Path, session_id: str = "test-persist") -> Path:
    """file 后端 aux 文件路径（pending_compact.json）。"""
    return tmp_dir / session_id / "pending_compact.json"


def _make_cm(
    tmp_dir: Path,
    session_id: str = "test-persist",
) -> ContextManager:
    messages: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir / session_id))
    compactor = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
    return ContextManager(
        session_id=session_id,
        messages=messages,
        system_prompt="You are a helpful assistant.",
        compactor=compactor,
    )


# ===================================================================
# 1. Write & Read
# ===================================================================


class TestPersistAndLoad:
    def test_persist_creates_file(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        result = PendingCompact(
            compact_content="[Compact] summary",
            start_uuid="start-123",
            end_uuid="end-456",
            usage=LLMUsage(prompt_tokens=5000, completion_tokens=200),
        )
        cm._persist_pending_compact(result)
        assert _aux_path(tmp_dir).exists()

    def test_load_recovers_data(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        result = PendingCompact(
            compact_content="[Compact] summary",
            start_uuid="start-123",
            end_uuid="end-456",
            usage=LLMUsage(prompt_tokens=5000, completion_tokens=200),
        )
        cm._persist_pending_compact(result)

        # Create a new CM to simulate session restart
        cm2 = _make_cm(tmp_dir)
        loaded = cm2._pending_compact_result
        assert loaded is not None
        assert loaded.compact_content == "[Compact] summary"
        assert loaded.start_uuid == "start-123"
        assert loaded.end_uuid == "end-456"
        assert loaded.usage is not None
        assert loaded.usage.prompt_tokens == 5000

    def test_load_no_file(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        assert cm._pending_compact_result is None


# ===================================================================
# 2. Delete
# ===================================================================


class TestDelete:
    def test_delete_removes_file(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        result = PendingCompact(
            compact_content="[Compact] x",
            start_uuid="s",
            end_uuid="e",
        )
        cm._persist_pending_compact(result)
        path = _aux_path(tmp_dir)
        assert path.exists()

        cm._delete_pending_compact()
        assert not path.exists()

    def test_delete_nonexistent_no_error(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        cm._delete_pending_compact()  # should not raise


# ===================================================================
# 3. Corrupt File Recovery
# ===================================================================


class TestCorruptRecovery:
    def test_corrupt_json_deleted(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        path = _aux_path(tmp_dir)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("not valid json{{{", encoding="utf-8")

        # Create new CM — should detect corrupt file and delete it
        cm2 = _make_cm(tmp_dir)
        assert cm2._pending_compact_result is None
        assert not path.exists()

    def test_missing_fields_deleted(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        path = _aux_path(tmp_dir)
        path.parent.mkdir(parents=True, exist_ok=True)
        # Missing required fields
        path.write_text(json.dumps({"compact_content": "x"}), encoding="utf-8")

        cm2 = _make_cm(tmp_dir)
        assert cm2._pending_compact_result is None
        assert not path.exists()


# ===================================================================
# 4. Session Restore Integration
# ===================================================================


class TestSessionRestore:
    def test_restore_on_init(self, tmp_dir):
        """ContextManager.__init__ loads pending compact from disk."""
        # First CM: persist a result
        cm1 = _make_cm(tmp_dir)
        result = PendingCompact(
            compact_content="[Compact] restored",
            start_uuid="abc",
            end_uuid="def",
        )
        cm1._persist_pending_compact(result)

        # Second CM: should load on init
        cm2 = _make_cm(tmp_dir)
        assert cm2._pending_compact_result is not None
        assert cm2._pending_compact_result.compact_content == "[Compact] restored"
