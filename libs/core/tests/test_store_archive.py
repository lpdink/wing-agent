"""归档区（archive）的存储契约：``FileSessionStore`` 的移动 / 索引 / 重建。

被守的语义（``SessionStore.archive / unarchive / list_archived``）：

- 归档 = **纯 rename**：目录从 live 区移入 ``.archive/``，字节不变；
- 索引（``.archive/index.json``）是列表的唯一读取口；缺失 / 损坏 → 按归档目录
  重建（``archived_at`` 留空——重建不知道当时的时刻，不发明数字）；目录已消失
  的条目被剔除并自愈；
- store 只守「状态机不允许的输入大声失败」：live / 归档双存在 → ``RuntimeError``，
  目录不存在 → ``FileNotFoundError``（幂等由会话层负责，不在这里静默吞）；
- ``.archive`` 是存储保留命名空间：不可寻址成会话、不进会话列表；
- memory 后端不具备归档能力（``archivable=False``；方法未被实现）。
"""

from __future__ import annotations

import json
import shutil
from pathlib import Path

import pytest

from wing.store import (
    ArchivedSession,
    FileSessionStore,
    MemorySessionStore,
    SessionMetadata,
)
from wing.store.file import FileMessageLog  # 实现细节：直测耐久语义


@pytest.fixture
def root(tmp_path: Path) -> Path:
    return tmp_path / "sessions"


@pytest.fixture
def store(root: Path) -> FileSessionStore:
    return FileSessionStore(root)


def _seed(
    store: FileSessionStore,
    session_id: str,
    *,
    name: str | None = None,
    tags: list[str] | None = None,
    records: list[dict] | None = None,
) -> None:
    """在 live 区写一个最小会话（metadata + history）。"""
    store.save_metadata(
        session_id, SessionMetadata(session_name=name, tags=tags or None)
    )
    store.open_log(session_id).append(
        records
        if records is not None
        else [{"role": "user", "content": "hello", "uuid": "u1"}]
    )


def _history_bytes(root: Path) -> bytes:
    return (root / "s1" / "history.jsonl").read_bytes()


def _index(store: FileSessionStore) -> list[dict]:
    return json.loads(
        (store.root / ".archive" / "index.json").read_text(encoding="utf-8")
    )["sessions"]


class TestArchiveRoundtrip:
    def test_archive_moves_dir_and_registers_index(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """归档：目录移动 + 索引登记（标题 / tags / 归档时间）；字节零变化。"""
        _seed(store, "s1", name="my session", tags=["task=x"])
        before = _history_bytes(root)

        store.archive("s1")

        assert not (root / "s1").exists()
        assert (root / ".archive" / "s1" / "history.jsonl").read_bytes() == before
        assert store.is_archived("s1") is True
        assert store.resolve_archived_id("s1") == "s1"

        entries = _index(store)
        assert len(entries) == 1
        entry = entries[0]
        assert entry["id"] == "s1"
        assert entry["name"] == "my session"
        assert entry["tags"] == ["task=x"]
        assert isinstance(entry["archived_at"], str) and entry["archived_at"]

    def test_title_falls_back_to_first_user_message(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """无名会话的归档条目带标题回退素材（与 live 区列表同一口径）。"""
        _seed(store, "s1", name=None)

        store.archive("s1")

        assert _index(store)[0]["name"] == "hello"

    def test_unarchive_restores_and_prunes_index(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """恢复：目录移动回来 + 索引条目删除；再次恢复前 live 区完整可读。"""
        _seed(store, "s1", name="my session")
        before = _history_bytes(root)
        store.archive("s1")

        store.unarchive("s1")

        assert (root / "s1" / "history.jsonl").read_bytes() == before
        assert store.is_archived("s1") is False
        assert _index(store) == []

    def test_archive_is_reserved_namespace(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """``.archive`` 与 ``.media`` 同类：不可寻址、不进会话列表、不算会话。"""
        _seed(store, "s1")
        store.archive("s1")

        ids = [s.id for s in store.list_summaries()]
        assert ids == []
        with pytest.raises(ValueError):
            store.resolve_stored_id(".archive")
        with pytest.raises(ValueError):
            store.exists(".archive")


class TestArchiveConflicts:
    def test_archive_missing_dir_raises(
        self, root: Path, store: FileSessionStore
    ) -> None:
        with pytest.raises(FileNotFoundError):
            store.archive("nope")

    def test_unarchive_missing_dir_raises(
        self, root: Path, store: FileSessionStore
    ) -> None:
        with pytest.raises(FileNotFoundError):
            store.unarchive("nope")

    def test_both_dirs_exist_raises(self, root: Path, store: FileSessionStore) -> None:
        """双存在异常态（手工拷贝等）：不猜谁对，两个方向都大声失败。"""
        _seed(store, "s1")
        store.archive("s1")
        (root / "s1").mkdir(parents=True, exist_ok=True)

        with pytest.raises(RuntimeError):
            store.archive("s1")
        with pytest.raises(RuntimeError):
            store.unarchive("s1")


class TestArchiveIndexRebuild:
    def test_missing_index_rebuilds_from_dirs(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """索引缺失（含从未写过）→ 按归档目录重建；archived_at 留空。"""
        _seed(store, "s1", name="my session")
        store.archive("s1")
        (root / ".archive" / "index.json").unlink()

        entries = store.list_archived()

        assert [e.id for e in entries] == ["s1"]
        assert entries[0].name == "my session"
        assert entries[0].archived_at is None
        # 重建落盘：下次读取走索引。
        assert _index(store)[0]["id"] == "s1"

    def test_corrupt_index_rebuilds(self, root: Path, store: FileSessionStore) -> None:
        _seed(store, "s1")
        store.archive("s1")
        (root / ".archive" / "index.json").write_text("{ not json", encoding="utf-8")

        assert [e.id for e in store.list_archived()] == ["s1"]

    def test_stale_entries_are_dropped_and_healed(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """条目对应的目录被手工删除 → 不再投影，索引自愈。"""
        _seed(store, "s1")
        _seed(store, "s2")
        store.archive("s1")
        store.archive("s2")
        shutil.rmtree(root / ".archive" / "s1")

        assert [e.id for e in store.list_archived()] == ["s2"]
        assert [e["id"] for e in _index(store)] == ["s2"]

    def test_index_batch_defers_and_applies_once(
        self, root: Path, store: FileSessionStore, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """批量窗口：块内攒、块尾一次落盘；空窗口不落盘；嵌套不改变语义。"""
        writes = 0
        real_write = store._write_archive_index

        def counting_write(entries) -> None:
            nonlocal writes
            writes += 1
            real_write(entries)

        monkeypatch.setattr(store, "_write_archive_index", counting_write)

        _seed(store, "a")
        _seed(store, "b")
        with store.index_batch():
            store.archive("a")
            store.archive("b")
            assert writes == 0, "块内不得落盘"
        assert writes == 1, "块尾一次落盘"
        assert sorted(e.id for e in store.list_archived()) == ["a", "b"]

        # 空窗口不落盘；恢复（删除条目）同样攒到块尾。
        with store.index_batch():
            pass
        assert writes == 1
        with store.index_batch():
            store.unarchive("a")
        assert writes == 2
        assert [e.id for e in store.list_archived()] == ["b"]

        # 嵌套：内层窗口不改变外层语义（外层兜底）。
        _seed(store, "c")
        with store.index_batch():
            store.archive("c")
            with store.index_batch():
                pass
            assert writes == 2
        assert writes == 3

    def test_listed_session_predicate_is_narrower_than_exists(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """展示判据（history 或带标签 metadata）比 exists（metadata 或 history）窄：
        只写过 metadata 的目录可解析/可水合，但不算"会话"、不进任何列表。"""
        store.save_metadata("meta-only", SessionMetadata(session_name="ghost"))
        assert store.exists("meta-only") is True
        assert store.is_listed_session("meta-only") is False

        store.save_metadata("tagged", SessionMetadata(tags=["favorite"]))
        assert store.is_listed_session("tagged") is True

        _seed(store, "chatty")
        assert store.is_listed_session("chatty") is True
        assert store.is_listed_session("../escape") is False

    def test_sealed_log_drops_writes(self, root: Path) -> None:
        """`MessageLog.seal`：封盘后 append / write_aux 都是 no-op（目录不重建）。"""
        log = FileMessageLog(root / "s1")
        log.append([{"role": "user", "content": "x"}])
        assert (root / "s1" / "history.jsonl").exists()

        log.seal()
        log.append([{"role": "user", "content": "y"}])
        log.write_aux("pending_compact", {"x": 1})

        assert [r["content"] for r in log.iter_all()] == ["x"]
        assert not (root / "s1" / "pending_compact.json").exists()

    def test_index_batch_replays_ops_in_order(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """窗口内的操作按序重放（last-wins）：不留重复条目，删除不被先前的
        upsert 盖回来。"""
        _seed(store, "a")
        store.archive("a")
        with store.index_batch():
            store.unarchive("a")  # remove
            store.archive("a")  # upsert
            store.unarchive("a")  # remove
            store.archive("a")  # upsert —— 最后一次是 upsert，且只留一条
        assert [e.id for e in store.list_archived()] == ["a"]
        assert len(_index(store)) == 1, _index(store)

        # 窗口内"归档后恢复"：条目必须消失（顺序语义，不是集合语义）。
        _seed(store, "b")
        with store.index_batch():
            store.archive("b")
            store.unarchive("b")
        assert [e.id for e in store.list_archived()] == ["a"]

    def test_batch_without_index_and_only_removals_writes_nothing(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """没有索引 + 窗口内只有删除 → 不落盘（读取路径不创造存储痕迹）。"""
        _seed(store, "a")
        store.archive("a")
        index = root / ".archive" / "index.json"
        index.unlink()  # 模拟索引丢失

        with store.index_batch():
            store.unarchive("a")

        assert not index.exists()

    def test_duplicate_index_entries_are_deduped_and_healed(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """手改索引造出重复条目：列表按磁盘真名去重并自愈落盘。"""
        _seed(store, "a")
        store.archive("a")
        index = root / ".archive" / "index.json"
        payload = json.loads(index.read_text(encoding="utf-8"))
        payload["sessions"].append(payload["sessions"][0])
        index.write_text(json.dumps(payload), encoding="utf-8")

        assert [e.id for e in store.list_archived()] == ["a"]
        assert len(_index(store)) == 1, _index(store)

    def test_index_with_a_dirty_id_is_ignored_not_projected(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """索引被手改出非法 id：不投影、不炸（逐条丢弃）。"""
        index = root / ".archive" / "index.json"
        index.parent.mkdir(parents=True)
        index.write_text(
            json.dumps({"sessions": [{"id": "../escape"}, {"id": "ghost"}]}),
            encoding="utf-8",
        )

        assert store.list_archived() == []

    def test_untracked_dirs_are_adopted_on_list(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """目录存在而索引未登记（手工拷入 / 索引写失败的窗口）→ 列表当场收编。"""
        _seed(store, "s1")
        store.archive("s1")
        # 手工"搬"一个新目录进归档区（源目录直接构造）。
        _seed(store, "s2")
        (root / "s2").rename(root / ".archive" / "s2")

        assert sorted(e.id for e in store.list_archived()) == ["s1", "s2"]
        # 收编落盘：下一次读取不再重复收编（索引里已有 s2）。
        assert sorted(e["id"] for e in _index(store)) == ["s1", "s2"]

    def test_untracked_non_session_dir_is_never_adopted(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """「不是会话」的目录（无 history、无标签）在收编与重建两侧都被跳过——
        同一盘上两次列表（索引在 / 索引重建后）结果必须一致。"""
        _seed(store, "s1")
        store.archive("s1")
        # 手工造一个"半成品"目录：只有 metadata，没有 history、没有 tags。
        ghost = root / ".archive" / "ghost-dir"
        ghost.mkdir()
        (ghost / "metadata.json").write_text(
            json.dumps({"session_name": "ghost"}), encoding="utf-8"
        )

        assert [e.id for e in store.list_archived()] == ["s1"]
        (root / ".archive" / "index.json").unlink()  # 重建路径
        assert [e.id for e in store.list_archived()] == ["s1"]

    def test_index_id_is_projected_as_the_on_disk_name(
        self, root: Path, store: FileSessionStore, monkeypatch
    ) -> None:
        """索引被写成别名（大小写 / Unicode 变体）时，列表展示磁盘真名。"""
        _seed(store, "Team-A")
        store.archive("Team-A")
        index = root / ".archive" / "index.json"
        payload = json.loads(index.read_text(encoding="utf-8"))
        payload["sessions"][0]["id"] = "team-a"  # 手改出的别名
        index.write_text(json.dumps(payload), encoding="utf-8")
        # 不依赖宿主 FS 是否大小写敏感：直接把解析固定成别名 → 真名。
        monkeypatch.setattr(
            store,
            "resolve_archived_id",
            lambda sid: "Team-A" if sid == "team-a" else None,
        )

        assert [e.id for e in store.list_archived()] == ["Team-A"]

    def test_dropped_and_adopted_entries_heal_together(
        self, root: Path, store: FileSessionStore
    ) -> None:
        """一次列表里同时剔除消失的条目、收编未登记的目录（索引自愈）。"""
        _seed(store, "s1")
        _seed(store, "s2")
        store.archive("s1")
        store.archive("s2")
        shutil.rmtree(root / ".archive" / "s1")  # s1 目录消失
        _seed(store, "s3")
        (root / "s3").rename(root / ".archive" / "s3")  # s3 未登记

        assert sorted(e.id for e in store.list_archived()) == ["s2", "s3"]
        assert sorted(e["id"] for e in _index(store)) == ["s2", "s3"]


class TestMemoryStoreHasNoArchive:
    def test_capability_flag_and_methods(self) -> None:
        memory = MemorySessionStore()
        assert memory.archivable is False
        assert memory.is_archived("s1") is False
        assert memory.resolve_archived_id("s1") is None
        with pytest.raises(NotImplementedError):
            memory.archive("s1")
        with pytest.raises(NotImplementedError):
            memory.unarchive("s1")
        with pytest.raises(NotImplementedError):
            memory.list_archived()

    def test_archived_session_model_roundtrip(self) -> None:
        """条目模型容忍未知字段（前向兼容：新版本写的索引旧版本能读）。"""
        entry = ArchivedSession.model_validate(
            {"id": "s1", "future_field": 1, "tags": ["pin"]}
        )
        assert entry.id == "s1"
        assert entry.tags == ["pin"]
