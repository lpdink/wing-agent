"""SessionStore 与 MessageLog 的单元测试：file / memory 两个后端。"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing.store import (
    FileSessionStore,
    MemorySessionStore,
    SessionMetadata,
    SessionStore,
)


@pytest.fixture(params=["file", "memory"])
def store(request: pytest.FixtureRequest, tmp_path: Path) -> SessionStore:
    if request.param == "file":
        return FileSessionStore(tmp_path / "sessions")
    return MemorySessionStore()


def _open_log(store: SessionStore, sid: str):
    return store.open_log(sid)


class TestMetadata:
    def test_roundtrip_full(self, store: SessionStore):
        meta = SessionMetadata(
            session_name="hello",
            workspace="/tmp/ws",
            last_interaction="2026-07-25T22:00:00",
            forked_from="20260101-000000-aaaaaaaa",
            template_name="coder",
        )
        store.save_metadata("sid-1", meta)
        loaded = store.load_metadata("sid-1")
        assert loaded == meta

    def test_load_missing_returns_none(self, store: SessionStore):
        assert store.load_metadata("no-such") is None

    def test_save_empty_is_no_record(self, store: SessionStore):
        store.save_metadata("sid-2", SessionMetadata())
        assert store.load_metadata("sid-2") is None

    def test_partial_fields(self, store: SessionStore):
        store.save_metadata("sid-3", SessionMetadata(workspace="/tmp/ws"))
        loaded = store.load_metadata("sid-3")
        assert loaded is not None
        assert loaded.workspace == "/tmp/ws"
        assert loaded.session_name is None
        assert loaded.forked_from is None


class TestFileCompat:
    """文件后端专属：老格式与未知字段兼容。"""

    def test_legacy_metadata_unknown_fields(self, tmp_path: Path):
        root = tmp_path / "sessions"
        sid = "20260101-000000-aaaaaaaa"
        session_dir = root / sid
        session_dir.mkdir(parents=True)
        (session_dir / "metadata.json").write_text(
            json.dumps({"workspace": "/old/ws", "some_future_field": 42}),
            encoding="utf-8",
        )
        store = FileSessionStore(root)
        loaded = store.load_metadata(sid)
        assert loaded is not None
        assert loaded.workspace == "/old/ws"
        assert loaded.session_name is None

    def test_corrupted_metadata_returns_empty(self, tmp_path: Path):
        root = tmp_path / "sessions"
        session_dir = root / "sid-bad"
        session_dir.mkdir(parents=True)
        (session_dir / "metadata.json").write_text("{not json", encoding="utf-8")
        store = FileSessionStore(root)
        loaded = store.load_metadata("sid-bad")
        assert loaded == SessionMetadata()


class TestMessageLog:
    def test_append_load_order(self, store: SessionStore):
        log = store.open_log("sid-log")
        records = [
            {"role": "user", "content": f"m{i}", "uuid": f"u{i}"} for i in range(5)
        ]
        log.append(records[:2])
        log.append(records[2:])
        assert log.load_all() == records

    def test_append_empty_is_nop(self, store: SessionStore):
        log = store.open_log("sid-empty")
        log.append([])
        assert log.load_all() == []

    def test_open_log_same_handle_content(self, store: SessionStore):
        log1 = store.open_log("sid-shared")
        log1.append([{"role": "user", "content": "x"}])
        log2 = store.open_log("sid-shared")
        assert len(log2.load_all()) == 1

    def test_snapshot_nop_or_written(self, store: SessionStore):
        log = store.open_log("sid-snap")
        log.write_snapshot([{"role": "user", "content": "x"}])
        # 不对快照做跨后端断言：file 写 newest.json，memory NOP。
        # file 布局在 TestFileLayout 中断言。


class TestAux:
    def test_write_read_delete(self, store: SessionStore):
        log = store.open_log("sid-aux")
        assert log.read_aux("pending_compact") is None
        log.write_aux("pending_compact", {"start_uuid": "a"})
        assert log.read_aux("pending_compact") == {"start_uuid": "a"}
        log.delete_aux("pending_compact")
        assert log.read_aux("pending_compact") is None

    def test_delete_missing_is_nop(self, store: SessionStore):
        log = store.open_log("sid-aux2")
        log.delete_aux("never-existed")

    def test_file_corrupted_aux_discarded(self, tmp_path: Path):
        from wing.store import FileMessageLog

        log = FileMessageLog(tmp_path / "sid-corrupt")
        log._path.mkdir(parents=True)
        (log._path / "pending_compact.json").write_text("{bad", encoding="utf-8")
        assert log.read_aux("pending_compact") is None
        assert not (log._path / "pending_compact.json").exists()


class TestExists:
    def _seed(self, store: SessionStore, *sids: str):
        for sid in sids:
            store.save_metadata(sid, SessionMetadata(session_name=sid))

    def test_exact_match(self, store: SessionStore):
        self._seed(store, "20260101-111111-aaaaaaaa")
        assert store.exists("20260101-111111-aaaaaaaa") is True

    def test_missing_returns_false(self, store: SessionStore):
        self._seed(store, "20260101-111111-aaaaaaaa")
        assert store.exists("zzz") is False

    def test_no_fuzzy_matching(self, store: SessionStore):
        """精确匹配：前缀/子串/通配符都不再解析（历史模糊匹配已移除）。"""
        self._seed(store, "20260101-111111-aaaaaaaa", "20260202-222222-bbbbbbbb")
        assert store.exists("20260101") is False
        assert store.exists("bbbbbbbb") is False
        assert store.exists("20260101*aaaaaaaa") is False

    def test_empty_session_not_exists(self, store: SessionStore):
        """仅 open_log 而未写入任何记录的 session 不算存在（两后端一致）。"""
        store.open_log("sid-empty")
        assert store.exists("sid-empty") is False
        store.open_log("sid-empty").append([{"role": "user", "content": "x"}])
        assert store.exists("sid-empty") is True


class TestListSummaries:
    def test_requires_messages(self, store: SessionStore):
        # 只有 metadata 没有消息 → 不列出
        store.save_metadata("sid-meta-only", SessionMetadata(session_name="x"))
        assert store.list_summaries() == []

    def test_name_from_metadata(self, store: SessionStore):
        store.save_metadata("sid-1", SessionMetadata(session_name="titled"))
        store.open_log("sid-1").append([{"role": "user", "content": "first"}])
        summaries = store.list_summaries()
        assert len(summaries) == 1
        assert summaries[0].id == "sid-1"
        assert summaries[0].metadata.session_name == "titled"
        assert summaries[0].first_user_message is None

    def test_first_user_message_fallback(self, store: SessionStore):
        store.save_metadata("sid-2", SessionMetadata(workspace="/ws"))
        store.open_log("sid-2").append(
            [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello " * 30},
            ]
        )
        summaries = store.list_summaries()
        assert len(summaries) == 1
        assert summaries[0].metadata.session_name is None
        assert summaries[0].first_user_message is not None
        assert len(summaries[0].first_user_message) <= 100
        assert summaries[0].first_user_message.startswith("hello")


class TestMemoryNoDisk:
    """memory 后端的任何操作都不产生文件。"""

    def test_full_lifecycle_no_files(self, tmp_path: Path):
        sentinel = tmp_path / "should-never-exist"
        store = MemorySessionStore()

        store.save_metadata("sid-m", SessionMetadata(session_name="m", workspace="/w"))
        log = store.open_log("sid-m")
        log.append([{"role": "user", "content": "hi", "uuid": "u1"}])
        log.write_snapshot([{"role": "user", "content": "hi"}])
        log.write_aux("pending_compact", {"start_uuid": "u1"})
        assert log.read_aux("pending_compact") is not None
        assert store.list_summaries()
        assert store.exists("sid-m") is True

        assert not sentinel.exists()
        # tmp_path 下没有任何 wing 产生的内容
        assert list(tmp_path.iterdir()) == []


class TestFileLayout:
    """文件后端保持既有磁盘布局。"""

    def test_layout_files(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        store.save_metadata("sid-l", SessionMetadata(session_name="l", workspace="/w"))
        log = store.open_log("sid-l")
        log.append([{"role": "user", "content": "hi"}])
        log.write_snapshot([{"role": "user", "content": "hi"}])
        log.write_aux("pending_compact", {"k": "v"})

        session_dir = root / "sid-l"
        assert (session_dir / "metadata.json").exists()
        assert (session_dir / "history.jsonl").exists()
        assert (session_dir / "newest.json").exists()
        assert (session_dir / "pending_compact.json").exists()

        meta = json.loads((session_dir / "metadata.json").read_text())
        assert meta == {"session_name": "l", "workspace": "/w"}

        lines = (session_dir / "history.jsonl").read_text().strip().splitlines()
        assert len(lines) == 1
        assert json.loads(lines[0])["content"] == "hi"

    def test_history_append_only(self, tmp_path: Path):
        store = FileSessionStore(tmp_path / "sessions")
        log = store.open_log("sid-a")
        log.append([{"role": "user", "content": "1"}])
        log.append([{"role": "assistant", "content": "2"}])
        lines = (tmp_path / "sessions" / "sid-a" / "history.jsonl").read_text()
        assert lines.count("\n") == 2
