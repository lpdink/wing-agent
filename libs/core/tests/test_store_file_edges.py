"""文件后端的边角：会话目录认领、根目录缺失、标题回退、aux 清理失败。

主流程在 test_session_store.py / test_session_*.py；这里补的是"磁盘上出现了
意外东西"时的降级：同名文件占位、只读挂载、根目录还没建、日志读不出来。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.store.base import IMAGE_ONLY_TITLE
from wing.store.file import FileMessageLog, FileSessionStore, _is_same_directory


@pytest.fixture
def store(tmp_path: Path) -> FileSessionStore:
    return FileSessionStore(tmp_path / "sessions")


def _write_history(store: FileSessionStore, session_id: str, lines: str) -> None:
    d = store._root / session_id
    d.mkdir(parents=True, exist_ok=True)
    (d / "history.jsonl").write_text(lines, encoding="utf-8")


class TestClaimSessionId:
    def test_plain_claim_creates_directory(self, store: FileSessionStore):
        assert store.claim_session_id("sess-1") == "sess-1"
        assert (store._root / "sess-1").is_dir()

    def test_claim_is_stable_on_second_call(self, store: FileSessionStore):
        store.claim_session_id("sess-1")
        assert store.claim_session_id("sess-1") == "sess-1"

    def test_path_occupied_by_file_falls_back_to_literal_id(
        self, store: FileSessionStore
    ):
        """同名**文件**占位（不是目录）：mkdir 抛 FileExistsError，回落逐字 id。"""
        store._root.mkdir(parents=True, exist_ok=True)
        (store._root / "sess-1").write_text("not a dir", encoding="utf-8")

        assert store.claim_session_id("sess-1") == "sess-1"

    def test_unwritable_root_warns_and_keeps_literal_id(
        self, store: FileSessionStore, monkeypatch, wing_warnings
    ):
        """只读挂载 / 权限不足：不把 create 打死，错误面留给真正的写盘处。"""
        store._root.mkdir(parents=True, exist_ok=True)
        original_mkdir = Path.mkdir

        def _denied(self: Path, *args, **kwargs):
            if self.name == "sess-ro":
                raise PermissionError("read-only file system")
            return original_mkdir(self, *args, **kwargs)

        monkeypatch.setattr(Path, "mkdir", _denied)

        assert store.claim_session_id("sess-ro") == "sess-ro"
        assert any("not created here" in m for m in wing_warnings)


class TestListSessionsEdges:
    def test_missing_root_returns_empty(self, tmp_path: Path):
        store = FileSessionStore(tmp_path / "never-created")

        assert store.list_summaries() == []

    def test_non_directory_entries_are_skipped(self, store: FileSessionStore):
        _write_history(store, "sess-1", '{"role": "user", "content": "hi"}\n')
        (store._root / "stray.txt").write_text("junk", encoding="utf-8")

        assert [s.id for s in store.list_summaries()] == ["sess-1"]

    def test_invalid_directory_names_are_skipped(self, store: FileSessionStore):
        """点命名空间（媒体池）与超长名字不是会话——闸门与寻址面同一把尺。"""
        _write_history(store, "sess-1", '{"role": "user", "content": "hi"}\n')
        _write_history(store, ".media", "x\n")
        _write_history(store, "x" * 200, "x\n")  # 超 SESSION_ID_MAX_BYTES

        assert [s.id for s in store.list_summaries()] == ["sess-1"]

    def test_empty_session_without_tags_is_hidden(self, store: FileSessionStore):
        (store._root / "empty").mkdir(parents=True, exist_ok=True)

        assert store.list_summaries() == []


class TestFirstUserMessage:
    def test_reads_first_user_line(self, store: FileSessionStore):
        _write_history(
            store,
            "sess-1",
            '{"role": "assistant", "content": "hi"}\n{"role": "user", "content": "ask"}\n',
        )

        assert store._first_user_message("sess-1") == "ask"

    def test_missing_log_returns_none(self, store: FileSessionStore):
        store.claim_session_id("sess-1")

        assert store._first_user_message("sess-1") is None

    def test_empty_content_is_skipped_for_later_text(self, store: FileSessionStore):
        """纯图消息（content 为空、media 非空）跳过，改取后面第一条非空文本。"""
        _write_history(
            store,
            "sess-1",
            '{"role": "user", "content": "", "media": [{"id": "aa"}]}\n'
            '{"role": "assistant", "content": "ok"}\n'
            '{"role": "user", "content": "later text"}\n',
        )

        assert store._first_user_message("sess-1") == "later text"

    def test_image_only_chain_falls_back_to_marker(self, store: FileSessionStore):
        """整条链只有纯图消息：回退名为短标记（不返回空串——空串会让会话被隐藏）。"""
        _write_history(
            store,
            "sess-1",
            '{"role": "user", "content": "", "media": [{"id": "bb"}]}\n',
        )

        assert store._first_user_message("sess-1") == IMAGE_ONLY_TITLE

    def test_unreadable_log_returns_none(self, store: FileSessionStore, monkeypatch):
        _write_history(store, "sess-1", '{"role": "user", "content": "ask"}\n')
        log = store.open_log("sess-1")

        def _boom():
            raise OSError("io error")

        monkeypatch.setattr(log, "iter_all", _boom)
        monkeypatch.setattr(store, "open_log", lambda session_id: log)

        assert store._first_user_message("sess-1") is None


class TestAuxEdges:
    def test_delete_aux_swallows_unlink_failure(
        self, tmp_path: Path, monkeypatch, wing_warnings
    ):
        log = FileMessageLog(tmp_path / "log")
        log.write_aux("k", {"a": 1})
        original_unlink = Path.unlink

        def _denied(self: Path, *args, **kwargs):
            raise OSError("locked")

        monkeypatch.setattr(Path, "unlink", _denied)
        try:
            log.delete_aux("k")  # 不抛
        finally:
            monkeypatch.setattr(Path, "unlink", original_unlink)

        assert log.read_aux("k") == {"a": 1}


class TestSameDirectoryProbe:
    def test_same_path_is_true(self, tmp_path: Path):
        assert _is_same_directory(tmp_path, tmp_path) is True

    def test_missing_candidate_is_false(self, tmp_path: Path):
        assert _is_same_directory(tmp_path / "gone", tmp_path) is False
