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
from wing.store.base import validate_media_content, validate_media_id

_GOOD_ID = "ab" * 32  # 64 位小写 hex（合法格式，未必是任何内容的 sha256）

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


def _mid(data: bytes) -> str:
    """真实内容地址（内容寻址：id 必须等于字节的 sha256）。"""
    return hashlib.sha256(data).hexdigest()


@pytest.fixture(params=["file", "memory"])
def store(request: pytest.FixtureRequest, tmp_path: Path) -> SessionStore:
    if request.param == "file":
        return FileSessionStore(tmp_path / "sessions")
    return MemorySessionStore()


class TestMediaRoundtrip:
    def test_write_read_roundtrip(self, store: SessionStore):
        data = b"\x89PNG\r\n\x1a\nbytes"
        store.write_media(_mid(data), data)
        assert store.read_media(_mid(data)) == data

    def test_missing_returns_none(self, store: SessionStore):
        # 合法的 64 位 hex，但没有对应对象
        assert store.read_media(_GOOD_ID) is None

    def test_write_is_idempotent(self, store: SessionStore):
        """内容寻址：同 id 重复写入幂等——首写即终值，第二次不报错也不改写。"""
        data = b"first"
        store.write_media(_mid(data), data)
        store.write_media(_mid(data), b"second")
        assert store.read_media(_mid(data)) == data

    def test_multiple_ids_isolated(self, store: SessionStore):
        one, two = b"one", b"two"
        store.write_media(_mid(one), one)
        store.write_media(_mid(two), two)
        assert store.read_media(_mid(one)) == one
        assert store.read_media(_mid(two)) == two

    def test_empty_bytes_roundtrip(self, store: SessionStore):
        mid = _mid(b"")
        store.write_media(mid, b"")
        assert store.read_media(mid) == b""


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


class TestContentAddressValidation:
    """首写校验 id 与字节一致（review r1 N2）。"""

    def test_write_rejects_mismatched_content(self, store: SessionStore):
        with pytest.raises(ValueError, match="内容寻址不一致"):
            store.write_media(_GOOD_ID, b"bytes-not-matching-the-id")

    def test_rejected_write_leaves_no_object(self, store: SessionStore):
        with pytest.raises(ValueError):
            store.write_media(_GOOD_ID, b"mismatch")
        assert store.read_media(_GOOD_ID) is None

    def test_existing_object_skips_content_check(self, store: SessionStore):
        """已存在 → 幂等跳过：不做内容校验（首写已校验），不抛不覆盖。"""
        data = b"first"
        mid = _mid(data)
        store.write_media(mid, data)
        store.write_media(mid, b"different")
        assert store.read_media(mid) == data

    def test_validator_direct(self):
        with pytest.raises(ValueError, match="内容寻址不一致"):
            validate_media_content("0" * 64, b"x")
        assert validate_media_content(_mid(b"x"), b"x") is None


class TestFileMediaLayout:
    def test_layout_path(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"hello"
        mid = _mid(data)
        store.write_media(mid, data)
        assert (root / ".media" / mid[:2] / mid).read_bytes() == data

    def test_no_tmp_leftover(self, tmp_path: Path):
        """原子写：落盘后不留 tmp 文件。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"data"
        mid = _mid(data)
        store.write_media(mid, data)
        files = sorted(p.name for p in (root / ".media" / mid[:2]).iterdir())
        assert files == [mid]

    def test_shared_pool_across_store_instances(self, tmp_path: Path):
        """同一 root 的多个 store 实例共享媒体池（池按存储根，不按 session）。"""
        root = tmp_path / "sessions"
        data = b"shared"
        mid = _mid(data)
        FileSessionStore(root).write_media(mid, data)
        assert FileSessionStore(root).read_media(mid) == data

    def test_unreadable_object_returns_none(self, tmp_path: Path):
        """损坏/不可读对象按"读不到"降级（None + WARN），不抛异常。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"data"
        mid = _mid(data)
        store.write_media(mid, data)
        path = root / ".media" / mid[:2] / mid
        path.unlink()
        path.mkdir()  # 同名目录：read_bytes 触发 IsADirectoryError
        assert store.read_media(mid) is None


class TestMemoryMediaPool:
    def test_pool_is_per_instance(self):
        """memory 后端是实例级共享池，不是进程级全局（避免测试/实例串味）。"""
        data = b"x"
        mid = _mid(data)
        a = MemorySessionStore()
        b = MemorySessionStore()
        a.write_media(mid, data)
        assert a.read_media(mid) == data
        assert b.read_media(mid) is None


class TestMediaNotASession:
    """`.media` 目录是存储根下的媒体池，不是 session——不得出现在会话列举里。"""

    def test_file_media_dir_not_listed(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"x"
        mid = _mid(data)
        store.write_media(mid, data)
        assert store.list_summaries() == []
        # 媒体 id（64 位 hex）在放宽后的会话 id 闸门里是**合法形态**（id 不透明，
        # 只防穿越与卫生），只是不存在 → False；真正危险的值仍然 raise。
        assert store.exists(mid) is False
        for unsafe in (".media", "../escape", "a/b", "a..b", "x" * 129):
            with pytest.raises(ValueError):
                store.exists(unsafe)

        sid = "20250101-000000-aaaaaaaa"
        store.save_metadata(sid, SessionMetadata(session_name="s"))
        store.open_log(sid).append([{"role": "user", "content": "hi"}])
        assert [s.id for s in store.list_summaries()] == [sid]
        assert store.exists(sid) is True

    def test_memory_media_does_not_create_session(self):
        store = MemorySessionStore()
        data = b"x"
        store.write_media(_mid(data), data)
        assert store.list_summaries() == []


class TestMediaSurvivesSessionLifecycle:
    def test_media_untouched_by_session_metadata_ops(self, store: SessionStore):
        """媒体池独立于会话目录/日志——会话的常规读写不清媒体。"""
        data = b"image"
        mid = _mid(data)
        sid = "20250101-000000-aaaaaaaa"
        store.save_metadata(sid, SessionMetadata(session_name="s"))
        store.open_log(sid).append([{"role": "user", "content": "hi"}])
        store.write_media(mid, data)
        store.save_metadata(sid, SessionMetadata(session_name="renamed"))
        store.open_log(sid).append([{"role": "assistant", "content": "yo"}])
        assert store.read_media(mid) == data
