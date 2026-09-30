"""SessionStore 媒体 API 单测：file / memory 两个后端。"""

from __future__ import annotations

import hashlib
from pathlib import Path
from typing import cast

import pytest

from wing.store import (
    FileSessionStore,
    MemorySessionStore,
    SessionMetadata,
    SessionStore,
)
from wing.store.base import validate_media_id

_GOOD_ID = "ab" * 32  # 64 位小写 hex
_OTHER_ID = "cd" * 32

_BAD_IDS = [
    "",
    "abc",
    "a" * 63,
    "a" * 65,
    "A" * 64,  # 大写
    "g" * 64,  # 非 hex
    ("../" * 2) + "a" * 60,  # 路径穿越形态
    "a" * 62 + "/b",
    "a" * 62 + "\\b",
    ".." * 32,
]


@pytest.fixture(params=["file", "memory"])
def store(request: pytest.FixtureRequest, tmp_path: Path) -> SessionStore:
    if request.param == "file":
        return FileSessionStore(tmp_path / "sessions")
    return MemorySessionStore()


class TestMediaRoundtrip:
    def test_write_read_roundtrip(self, store: SessionStore):
        store.write_media(_GOOD_ID, b"\x89PNG\r\n\x1a\nbytes")
        assert store.read_media(_GOOD_ID) == b"\x89PNG\r\n\x1a\nbytes"

    def test_missing_returns_none(self, store: SessionStore):
        assert store.read_media(_GOOD_ID) is None

    def test_write_is_idempotent(self, store: SessionStore):
        """内容寻址：同 id 重复写入幂等——首写即终值，第二次不报错也不改写。"""
        store.write_media(_GOOD_ID, b"first")
        store.write_media(_GOOD_ID, b"second")
        assert store.read_media(_GOOD_ID) == b"first"

    def test_multiple_ids_isolated(self, store: SessionStore):
        store.write_media(_GOOD_ID, b"one")
        store.write_media(_OTHER_ID, b"two")
        assert store.read_media(_GOOD_ID) == b"one"
        assert store.read_media(_OTHER_ID) == b"two"

    def test_empty_bytes_roundtrip(self, store: SessionStore):
        empty_id = hashlib.sha256(b"").hexdigest()
        store.write_media(empty_id, b"")
        assert store.read_media(empty_id) == b""


class TestInvalidIdRejected:
    def test_write_rejects(self, store: SessionStore):
        for bad in _BAD_IDS:
            with pytest.raises(ValueError):
                store.write_media(bad, b"x")

    def test_read_rejects(self, store: SessionStore):
        for bad in _BAD_IDS:
            with pytest.raises(ValueError):
                store.read_media(bad)

    def test_validator_rejects_non_str(self):
        with pytest.raises(ValueError):
            validate_media_id(cast(str, 42))

    def test_validator_accepts_and_returns(self):
        assert validate_media_id(_GOOD_ID) == _GOOD_ID


class TestFileMediaLayout:
    def test_layout_path(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        mid = hashlib.sha256(b"hello").hexdigest()
        store.write_media(mid, b"hello")
        assert (root / ".media" / mid[:2] / mid).read_bytes() == b"hello"

    def test_no_tmp_leftover(self, tmp_path: Path):
        """原子写：落盘后不留 tmp 文件。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        store.write_media(_GOOD_ID, b"data")
        files = sorted(p.name for p in (root / ".media" / _GOOD_ID[:2]).iterdir())
        assert files == [_GOOD_ID]

    def test_shared_pool_across_store_instances(self, tmp_path: Path):
        """同一 root 的多个 store 实例共享媒体池（池按存储根，不按 session）。"""
        root = tmp_path / "sessions"
        FileSessionStore(root).write_media(_GOOD_ID, b"shared")
        assert FileSessionStore(root).read_media(_GOOD_ID) == b"shared"

    def test_unreadable_object_returns_none(self, tmp_path: Path):
        """损坏/不可读对象按"读不到"降级（None + WARN），不抛异常。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        store.write_media(_GOOD_ID, b"data")
        path = root / ".media" / _GOOD_ID[:2] / _GOOD_ID
        path.unlink()
        path.mkdir()  # 同名目录：read_bytes 触发 IsADirectoryError
        assert store.read_media(_GOOD_ID) is None


class TestMemoryMediaPool:
    def test_pool_is_per_instance(self):
        """memory 后端是实例级共享池，不是进程级全局（避免测试/实例串味）。"""
        a = MemorySessionStore()
        b = MemorySessionStore()
        a.write_media(_GOOD_ID, b"x")
        assert a.read_media(_GOOD_ID) == b"x"
        assert b.read_media(_GOOD_ID) is None


class TestMediaNotASession:
    """`.media` 目录是存储根下的媒体池，不是 session——不得出现在会话列举里。"""

    def test_file_media_dir_not_listed(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        store.write_media(_GOOD_ID, b"x")
        assert store.list_summaries() == []
        assert store.exists(_GOOD_ID) is False

        store.save_metadata("sid-1", SessionMetadata(session_name="s"))
        store.open_log("sid-1").append([{"role": "user", "content": "hi"}])
        assert [s.id for s in store.list_summaries()] == ["sid-1"]
        assert store.exists("sid-1") is True

    def test_memory_media_does_not_create_session(self):
        store = MemorySessionStore()
        store.write_media(_GOOD_ID, b"x")
        assert store.list_summaries() == []


class TestMediaSurvivesSessionLifecycle:
    def test_media_untouched_by_session_metadata_ops(self, store: SessionStore):
        """媒体池独立于会话目录/日志——会话的常规读写不清媒体。"""
        store.save_metadata("sid-1", SessionMetadata(session_name="s"))
        store.open_log("sid-1").append([{"role": "user", "content": "hi"}])
        store.write_media(_GOOD_ID, b"image")
        store.save_metadata("sid-1", SessionMetadata(session_name="renamed"))
        store.open_log("sid-1").append([{"role": "assistant", "content": "yo"}])
        assert store.read_media(_GOOD_ID) == b"image"
