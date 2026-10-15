"""SessionStore 媒体 API 单测：file / memory 两个后端。

模型：媒体对象是**会话私有**资产（file 后端 `<root>/<sid>/media/<id>`），
会话目录自包含；遗留共享池（`<root>/.media/<id[:2]>/<id>`）只读兼容——
读 miss 回退并物化（幂等）。永不删除任何对象。
"""

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

_SID = "20250101-000000-aaaaaaaa"
_SID_OTHER = "20250101-000000-bbbbbbbb"

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

_BAD_SESSION_IDS = ["", "../escape", "a/b", ".media", "x" * 129]


def _mid(data: bytes) -> str:
    """真实内容地址（内容寻址：id 必须等于字节的 sha256）。"""
    return hashlib.sha256(data).hexdigest()


def _legacy_path(root: Path, media_id: str) -> Path:
    """遗留共享池路径（只读兼容面）。"""
    return root / ".media" / media_id[:2] / media_id


def _seed_legacy(root: Path, data: bytes) -> str:
    """在遗留池里手工放一个对象（模拟迁移前落盘的老会话）。"""
    mid = _mid(data)
    path = _legacy_path(root, mid)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return mid


@pytest.fixture(params=["file", "memory"])
def store(request: pytest.FixtureRequest, tmp_path: Path) -> SessionStore:
    if request.param == "file":
        return FileSessionStore(tmp_path / "sessions")
    return MemorySessionStore()


class TestMediaRoundtrip:
    def test_write_read_roundtrip(self, store: SessionStore):
        data = b"\x89PNG\r\n\x1a\nbytes"
        store.write_media(_SID, _mid(data), data)
        assert store.read_media(_SID, _mid(data)) == data

    def test_missing_returns_none(self, store: SessionStore):
        # 合法的 64 位 hex，但没有对应对象
        assert store.read_media(_SID, _GOOD_ID) is None

    def test_write_is_idempotent(self, store: SessionStore):
        """内容寻址：同 id 重复写入幂等——首写即终值，第二次不报错也不改写。"""
        data = b"first"
        store.write_media(_SID, _mid(data), data)
        store.write_media(_SID, _mid(data), b"second")
        assert store.read_media(_SID, _mid(data)) == data

    def test_multiple_ids_isolated(self, store: SessionStore):
        one, two = b"one", b"two"
        store.write_media(_SID, _mid(one), one)
        store.write_media(_SID, _mid(two), two)
        assert store.read_media(_SID, _mid(one)) == one
        assert store.read_media(_SID, _mid(two)) == two

    def test_empty_bytes_roundtrip(self, store: SessionStore):
        mid = _mid(b"")
        store.write_media(_SID, mid, b"")
        assert store.read_media(_SID, mid) == b""


class TestSessionPrivateScope:
    """媒体是会话私有资产：另一个会话看不到（自包含语义）。"""

    def test_other_session_cannot_read(self, store: SessionStore):
        data = b"private"
        mid = _mid(data)
        store.write_media(_SID, mid, data)
        assert store.read_media(_SID_OTHER, mid) is None
        assert store.media_exists(_SID_OTHER, mid) is False
        assert store.materialize(_SID_OTHER, mid) is False

    def test_same_media_in_two_sessions(self, store: SessionStore):
        """同一张图在两个会话各有一份（各自可见，无去重承诺）。"""
        data = b"shared-bytes"
        mid = _mid(data)
        store.write_media(_SID, mid, data)
        store.write_media(_SID_OTHER, mid, data)
        assert store.read_media(_SID, mid) == data
        assert store.read_media(_SID_OTHER, mid) == data


class TestInvalidIdRejected:
    def test_write_rejects_bad_media_id(self, store: SessionStore):
        for bad in _BAD_IDS:
            with pytest.raises(ValueError):
                store.write_media(_SID, bad, b"x")

    def test_methods_reject_bad_media_id(self, store: SessionStore):
        for bad in _BAD_IDS:
            with pytest.raises(ValueError):
                store.read_media(_SID, bad)
            with pytest.raises(ValueError):
                store.media_exists(_SID, bad)
            with pytest.raises(ValueError):
                store.media_path(_SID, bad)
            with pytest.raises(ValueError):
                store.materialize(_SID, bad)

    def test_methods_reject_bad_session_id(self, store: SessionStore):
        """session id 也是路径组件：闸门先于一切（防穿越）。"""
        for bad in _BAD_SESSION_IDS:
            with pytest.raises(ValueError):
                store.write_media(bad, _GOOD_ID, b"x")
            with pytest.raises(ValueError):
                store.read_media(bad, _GOOD_ID)
            with pytest.raises(ValueError):
                store.media_exists(bad, _GOOD_ID)
            with pytest.raises(ValueError):
                store.media_path(bad, _GOOD_ID)
            with pytest.raises(ValueError):
                store.materialize(bad, _GOOD_ID)

    def test_validator_rejects_non_str(self):
        with pytest.raises(ValueError):
            validate_media_id(cast(str, 42))

    def test_validator_accepts_and_returns(self):
        assert validate_media_id(_GOOD_ID) == _GOOD_ID


class TestContentAddressValidation:
    """首写校验 id 与字节一致。"""

    def test_write_rejects_mismatched_content(self, store: SessionStore):
        with pytest.raises(ValueError, match="内容寻址不一致"):
            store.write_media(_SID, _GOOD_ID, b"bytes-not-matching-the-id")

    def test_rejected_write_leaves_no_object(self, store: SessionStore):
        with pytest.raises(ValueError):
            store.write_media(_SID, _GOOD_ID, b"mismatch")
        assert store.read_media(_SID, _GOOD_ID) is None

    def test_existing_object_skips_content_check(self, store: SessionStore):
        """已存在 → 幂等跳过：不做内容校验（首写已校验），不抛不覆盖。"""
        data = b"first"
        mid = _mid(data)
        store.write_media(_SID, mid, data)
        store.write_media(_SID, mid, b"different")
        assert store.read_media(_SID, mid) == data

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
        store.write_media(_SID, mid, data)
        assert (root / _SID / "media" / mid).read_bytes() == data

    def test_no_tmp_leftover(self, tmp_path: Path):
        """原子写：落盘后不留 tmp 文件。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"data"
        mid = _mid(data)
        store.write_media(_SID, mid, data)
        files = sorted(p.name for p in (root / _SID / "media").iterdir())
        assert files == [mid]

    def test_legacy_pool_never_written(self, tmp_path: Path):
        """写路径只写会话目录：老池目录整个不出现。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        store.write_media(_SID, _mid(b"x"), b"x")
        assert not (root / ".media").exists()

    def test_session_dir_shared_across_store_instances(self, tmp_path: Path):
        """同一 root 的多个 store 实例读写同一份会话目录（存储按 root 定位）。"""
        root = tmp_path / "sessions"
        data = b"shared"
        mid = _mid(data)
        FileSessionStore(root).write_media(_SID, mid, data)
        assert FileSessionStore(root).read_media(_SID, mid) == data

    def test_unreadable_object_returns_none(self, tmp_path: Path):
        """损坏/不可读对象按"读不到"降级（None + WARN），不抛异常。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"data"
        mid = _mid(data)
        store.write_media(_SID, mid, data)
        path = root / _SID / "media" / mid
        path.unlink()
        path.mkdir()  # 同名目录：read_bytes 触发 IsADirectoryError
        assert store.read_media(_SID, mid) is None


class TestLegacyPoolFallback:
    """老池只读兼容：读 miss 回退老池并物化（幂等），对象永不删除。"""

    def test_read_falls_back_and_materializes(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"legacy-bytes"
        mid = _seed_legacy(root, data)

        # 物化前：会话目录里没有对象，但两处可寻址面都已经"看得见"。
        assert (root / _SID / "media" / mid).exists() is False
        assert store.media_exists(_SID, mid) is True
        assert store.media_path(_SID, mid) == str(_legacy_path(root, mid))

        # 读取：返回字节 + 物化进会话目录；老对象原样保留（永不删除）。
        assert store.read_media(_SID, mid) == data
        assert (root / _SID / "media" / mid).read_bytes() == data
        assert _legacy_path(root, mid).read_bytes() == data
        assert store.media_path(_SID, mid) == str(root / _SID / "media" / mid)

    def test_materialize_is_idempotent(self, tmp_path: Path):
        """二次物化不重写（快路径 stat 命中）：inode 不变。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"once"
        mid = _seed_legacy(root, data)

        assert store.materialize(_SID, mid) is True
        target = root / _SID / "media" / mid
        inode = target.stat().st_ino
        assert store.materialize(_SID, mid) is True
        assert target.stat().st_ino == inode

    def test_second_read_does_not_rewrite(self, tmp_path: Path):
        """读路径二次读取同样不重写（目标已在 → 直接读）。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"twice"
        mid = _seed_legacy(root, data)

        assert store.read_media(_SID, mid) == data
        target = root / _SID / "media" / mid
        inode = target.stat().st_ino
        assert store.read_media(_SID, mid) == data
        assert target.stat().st_ino == inode

    def test_materialize_missing_returns_false(self, tmp_path: Path):
        store = FileSessionStore(tmp_path / "sessions")
        assert store.materialize(_SID, _GOOD_ID) is False
        assert store.media_path(_SID, _GOOD_ID) is None

    def test_materialize_does_not_touch_legacy_copy(self, tmp_path: Path):
        """物化是复制而非搬迁：老池对象留在原处（不删任何东西）。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"keep"
        mid = _seed_legacy(root, data)
        store.materialize(_SID, mid)
        assert _legacy_path(root, mid).read_bytes() == data

    def test_legacy_read_failure_returns_none(self, tmp_path: Path):
        """老池对象不可读 → 与既有"读不到"同价（WARN + None），不抛。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        mid = _GOOD_ID
        legacy = _legacy_path(root, mid)
        legacy.mkdir(parents=True)  # 同名目录
        assert store.read_media(_SID, mid) is None
        assert store.materialize(_SID, mid) is False

    def test_materialize_write_failure_does_not_block_read(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """物化失败（磁盘满 / 权限）：本次读取照常返回字节，materialize 返回 False。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"fallback"
        mid = _seed_legacy(root, data)

        def boom(path: Path, payload: bytes) -> None:
            raise OSError(28, "No space left on device")

        monkeypatch.setattr("wing.store.file.atomic_write_bytes", boom)

        assert store.read_media(_SID, mid) == data
        assert store.materialize(_SID, mid) is False
        assert (root / _SID / "media" / mid).exists() is False


class TestMediaQuerySurface:
    def test_exists_and_path_matrix(self, store: SessionStore, tmp_path: Path):
        data = b"query"
        mid = _mid(data)
        assert store.media_exists(_SID, mid) is False
        assert store.media_path(_SID, mid) is None

        store.write_media(_SID, mid, data)
        assert store.media_exists(_SID, mid) is True
        path = store.media_path(_SID, mid)
        if isinstance(store, MemorySessionStore):
            # memory 后端没有本地文件：路径面恒为 None（消费方降级）。
            assert path is None
        else:
            assert isinstance(path, str)
            assert path == str(tmp_path / "sessions" / _SID / "media" / mid)
            assert Path(path).read_bytes() == data

    def test_exists_does_not_materialize(self, tmp_path: Path):
        """存在性查询无副作用：老池命中也不物化（物化归加载 / 读路径）。"""
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        mid = _seed_legacy(root, b"lazy")
        assert store.media_exists(_SID, mid) is True
        assert store.media_path(_SID, mid) is not None
        assert (root / _SID / "media" / mid).exists() is False


class TestMemoryMediaScope:
    def test_bucket_is_per_instance(self):
        """memory 后端是实例级（不是进程级全局）——避免测试 / 实例串味。"""
        data = b"x"
        mid = _mid(data)
        a = MemorySessionStore()
        b = MemorySessionStore()
        a.write_media(_SID, mid, data)
        assert a.read_media(_SID, mid) == data
        assert b.read_media(_SID, mid) is None

    def test_materialize_is_membership(self):
        """memory 没有老池：materialize 真值 == 该会话桶内已有此对象。"""
        data = b"member"
        mid = _mid(data)
        store = MemorySessionStore()
        assert store.materialize(_SID, mid) is False
        store.write_media(_SID, mid, data)
        assert store.materialize(_SID, mid) is True


class TestMediaNotASession:
    """媒体对象 / 老池目录都不是 session——不得出现在会话列举里。"""

    def test_file_media_dir_not_listed(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        data = b"x"
        mid = _mid(data)
        store.write_media(_SID, mid, data)
        _seed_legacy(root, b"legacy")

        # 媒体目录（无论会话私有还是老池）都不算会话：写媒体不创造会话。
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
        store.write_media(_SID, _mid(data), data)
        assert store.list_summaries() == []
        assert store.exists(_SID) is False


class TestMediaSurvivesSessionLifecycle:
    def test_media_untouched_by_session_metadata_ops(self, store: SessionStore):
        """媒体独立于会话元数据 / 日志——会话的常规读写不清媒体。"""
        data = b"image"
        mid = _mid(data)
        store.save_metadata(_SID, SessionMetadata(session_name="s"))
        store.open_log(_SID).append([{"role": "user", "content": "hi"}])
        store.write_media(_SID, mid, data)
        store.save_metadata(_SID, SessionMetadata(session_name="renamed"))
        store.open_log(_SID).append([{"role": "assistant", "content": "yo"}])
        assert store.read_media(_SID, mid) == data
