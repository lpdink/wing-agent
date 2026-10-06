"""会话标签（tags）单元测试。

覆盖：标签词汇校验与增删原语（``session/tags.py`` 纯函数）、
``SessionManager.set_session_tags`` 的两条路径（在内存 / 不在内存——后者
**不水合**、直接改磁盘 metadata）、创建即打标、列表投影、fork 不继承、
落盘洁癖（空标签不落字段、no-op 不产生写）。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing.schema import Message
from wing.session import SessionManager
from wing.session.tags import (
    MAX_TAG_LENGTH,
    MAX_TAGS_PER_SESSION,
    apply_tag_ops,
    sanitize_tags,
    validate_tag,
)
from wing.store import FileSessionStore, SessionMetadata


@pytest.fixture
def file_sm(tmp_path: Path) -> SessionManager:
    """文件后端 SessionManager。"""
    return SessionManager({"file": FileSessionStore(tmp_path / "sessions")})


def _seed(session, *contents: str) -> None:
    """向 session 的上下文写入消息（不经 LLM）。"""
    for i, content in enumerate(contents):
        role = "user" if i % 2 == 0 else "assistant"
        session.context_manager.add_message(Message(role=role, content=content))


class TestValidateTag:
    def test_accepts_conventions_and_unicode(self):
        """裸词 / k=v / CJK / 点号中线都合法（不透明字符串，不做语义解析）。"""
        for tag in ("scheduler", "task=wing-tag", "收藏", "a.b-c_d", "l1"):
            assert validate_tag(tag) == tag

    @pytest.mark.parametrize(
        "tag",
        [
            "",
            "has space",
            "has\ttab",
            "has,comma",
            "-leading-dash",
            "x" * (MAX_TAG_LENGTH + 1),
            "line\nbreak",
        ],
    )
    def test_rejects_invalid(self, tag: str):
        with pytest.raises(ValueError):
            validate_tag(tag)


class TestApplyTagOps:
    def test_add_dedupes_and_preserves_order(self):
        m = apply_tag_ops(None, add=["b", "a", "b"])
        assert m.tags == ["b", "a"]
        assert m.added == ["b", "a"]
        assert m.removed == []

    def test_idempotent_repeat_adds_and_removes(self):
        m = apply_tag_ops(["a"], add=["a"], remove=["nope"])
        assert m.tags == ["a"]
        assert m.added == []
        assert m.removed == []

    def test_remove_keeps_original_order(self):
        m = apply_tag_ops(["a", "b", "c"], remove=["b"])
        assert m.tags == ["a", "c"]
        assert m.removed == ["b"]

    def test_remove_wins_when_tag_on_both_sides_net_delta_empty(self):
        """同一标签同时在 add / remove：先加后删 remove 胜出；净变化两侧皆空。

        added / removed 报**净变化**（不是操作流）——净 no-op 不触发写盘。
        """
        m = apply_tag_ops(None, add=["x"], remove=["x"])
        assert m.tags == []
        assert m.added == []
        assert m.removed == []

    def test_net_delta_against_existing_base(self):
        """净变化相对既有集合计算：既有 [x]，add=[x] remove=[] → 无净变化。"""
        m = apply_tag_ops(["x"], add=["x"])
        assert m.tags == ["x"]
        assert m.added == [] and m.removed == []

    def test_read_only_call_returns_copy(self):
        m = apply_tag_ops(["a"])
        assert m.tags == ["a"]
        assert not m.added and not m.removed

    def test_cap_blocks_expansion_only(self):
        """上限只拦扩张：存量超限（损坏 / 手改）允许读取与收缩，不允许再增加。"""
        full = [f"t{i}" for i in range(MAX_TAGS_PER_SESSION)]
        with pytest.raises(ValueError):
            apply_tag_ops(full, add=["one-more"])

        oversized = [f"t{i}" for i in range(MAX_TAGS_PER_SESSION + 3)]
        assert apply_tag_ops(oversized).tags == oversized  # 读取放行
        shrunk = apply_tag_ops(oversized, remove=["t0", "t1", "t2"])
        assert len(shrunk.tags) == MAX_TAGS_PER_SESSION  # 收缩放行
        with pytest.raises(ValueError):
            apply_tag_ops(oversized, add=["one-more"])  # 扩张拦截

    def test_invalid_input_never_silently_dropped(self):
        with pytest.raises(ValueError):
            apply_tag_ops(None, add=["ok", "bad tag"])


class TestSanitizeTags:
    """读侧清洗：存量脏数据的投影归一（绝不 raise）。"""

    def test_dedupes_and_drops_invalid_keeps_order(self):
        assert sanitize_tags(["a", "a", "bad tag", "b", "", "-lead", "a=b", "c"]) == [
            "a",
            "b",
            "a=b",
            "c",
        ]

    def test_none_and_empty(self):
        assert sanitize_tags(None) == []
        assert sanitize_tags([]) == []

    def test_applied_as_mutation_base(self):
        """apply_tag_ops 的既有集合先清洗：脏数据不阻断变更，且写出的结果干净。"""
        m = apply_tag_ops(["ok", "bad tag", "ok"], add=["new"])
        assert m.tags == ["ok", "new"]
        assert m.added == ["new"]


class TestManagerSetTags:
    @pytest.mark.asyncio
    async def test_set_tags_on_loaded_session_persists(self, file_sm: SessionManager):
        session = file_sm.create_session()
        _seed(session, "hello")

        m = file_sm.set_session_tags(
            session.session_id, add=["favorite", "task=wing-tag"]
        )
        assert m.tags == ["favorite", "task=wing-tag"]
        assert session.tags == ["favorite", "task=wing-tag"]

        meta = session.store.load_metadata(session.session_id)
        assert meta is not None and meta.tags == ["favorite", "task=wing-tag"]

    @pytest.mark.asyncio
    async def test_set_tags_on_evicted_session_does_not_hydrate(
        self, file_sm: SessionManager, tmp_path: Path
    ):
        """逐出会话直接改磁盘 metadata：不水合、不写多余键、标签可读。"""
        session = file_sm.create_session()
        _seed(session, "hello")
        sid = session.session_id
        # 逐出（等价于不在内存；evict 的拆解语义与标签路径无关）
        del file_sm._sessions[sid]

        m = file_sm.set_session_tags(sid, add=["favorite"])
        assert m.tags == ["favorite"]
        # 不水合：调用后会话仍在内存之外
        assert sid not in file_sm._sessions

        # 磁盘事实：metadata.json 里只有 tags 与必要字段（未创建会话对象）
        meta = session.store.load_metadata(sid)
        assert meta is not None and meta.tags == ["favorite"]
        assert meta.template_name is None and meta.workspace is None

        # 纯读同样不水合
        read = file_sm.set_session_tags(sid)
        assert read.tags == ["favorite"]
        assert sid not in file_sm._sessions

    @pytest.mark.asyncio
    async def test_noop_mutation_does_not_write(
        self, file_sm: SessionManager, monkeypatch: pytest.MonkeyPatch
    ):
        """幂等 no-op（重复添加 / 移除不存在）不产生任何落盘。"""
        session = file_sm.create_session()
        _seed(session, "hello")
        file_sm.set_session_tags(session.session_id, add=["a"])

        calls: list[str] = []
        orig = session.store.save_metadata
        monkeypatch.setattr(
            session.store,
            "save_metadata",
            lambda sid, meta: calls.append(sid) or orig(sid, meta),
        )

        m = file_sm.set_session_tags(session.session_id, add=["a"], remove=["none"])
        assert not m.added and not m.removed
        assert calls == []

    @pytest.mark.asyncio
    async def test_unknown_session_raises_lookup_error(self, file_sm: SessionManager):
        with pytest.raises(LookupError):
            file_sm.set_session_tags("20250101-000000-abcdef03", add=["x"])

    @pytest.mark.asyncio
    async def test_malformed_session_id_never_reaches_store(
        self, file_sm: SessionManager
    ):
        """格式闸门：不合规 id 与"不存在"同价（LookupError），不触碰文件系统。"""
        for bad in (
            "sid-1",
            "../escape",
            "../../etc/passwd",
            "/tmp/absolute",
            "20250101-000000-ABCDEF01",  # 大写 hex 不合规（严格小写）
            "20250101-000000-abcdef0",  # 长度不足
            "20260101-111111",  # 前缀（截断）不合规
        ):
            with pytest.raises(LookupError):
                file_sm.set_session_tags(bad, add=["x"])

    @pytest.mark.asyncio
    async def test_invalid_tag_raises_before_any_write(self, file_sm: SessionManager):
        session = file_sm.create_session()
        _seed(session, "hello")
        with pytest.raises(ValueError):
            file_sm.set_session_tags(session.session_id, add=["bad tag"])
        meta = session.store.load_metadata(session.session_id)
        assert meta is None or not meta.tags


class TestStorePathClearLastTag:
    """B1 回归：store 路径（未加载会话）清掉最后一个标签必须真的落盘。

    旧缺陷：这次变更把 metadata 整体清空（tags-only 记录）时，
    ``save_metadata`` 因"序列化后为空"早退 → 端点回报 removed、磁盘却保留
    旧 tags → 下一次读取"复活"。清空必须是可持久化的显式事实。
    """

    @pytest.mark.asyncio
    async def test_clear_last_tag_on_history_only_session(self, tmp_path: Path):
        """只有 history.jsonl 的会话（崩溃窗口 / 老数据）add 后再 remove。"""
        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})
        sid = "20250101-000000-abcdef01"
        session_dir = tmp_path / "sessions" / sid
        session_dir.mkdir(parents=True)
        (session_dir / "history.jsonl").write_text(
            '{"role": "user", "content": "hi", "uuid": "u1", "parent_uuid": null}\n',
            encoding="utf-8",
        )

        added = sm.set_session_tags(sid, add=["favorite"])
        assert added.tags == ["favorite"]
        assert sid not in sm._sessions
        # store 路径写出的 tags-only 记录（未水合，故无 template/workspace 等）
        assert json.loads((session_dir / "metadata.json").read_text()) == {
            "tags": ["favorite"]
        }

        removed = sm.set_session_tags(sid, remove=["favorite"])
        assert removed.tags == [] and removed.removed == ["favorite"]
        # 磁盘事实：真的清掉了（不是"端点说清掉了"）；重读不复活。
        assert sm.set_session_tags(sid).tags == []
        assert "tags" not in json.loads((session_dir / "metadata.json").read_text())

    @pytest.mark.asyncio
    async def test_clear_last_tag_on_memory_store(self):
        """memory 后端同型语义：清空落位，不复活。"""
        from wing.store import MemorySessionStore

        store = MemorySessionStore()
        sm = SessionManager({"memory": store}, default_backend="memory")
        sid = "20250101-000000-abcdef02"
        store.save_metadata(sid, SessionMetadata(tags=["favorite"]))

        removed = sm.set_session_tags(sid, remove=["favorite"])
        assert removed.tags == [] and removed.removed == ["favorite"]
        assert sm.set_session_tags(sid).tags == []
        meta = store.load_metadata(sid)
        assert meta is not None and meta.tags is None


class TestCreateAndProjection:
    @pytest.mark.asyncio
    async def test_create_with_tags(self, file_sm: SessionManager):
        session = file_sm.create_session(tags=["scheduler", "task=x"])
        _seed(session, "hello")  # 列表只收"有标题素材"的会话（既有语义）
        assert session.tags == ["scheduler", "task=x"]

        infos = {i.id: i for i in file_sm.list_sessions()}
        assert infos[session.session_id].tags == ["scheduler", "task=x"]

    @pytest.mark.asyncio
    async def test_tagged_session_without_messages_is_listed(
        self, file_sm: SessionManager
    ):
        """带标会话在首条消息落盘前就可被列表找到（"创建即打标"的窗口期）。"""
        session = file_sm.create_session(tags=["wing-probe"])

        infos = {i.id: i for i in file_sm.list_sessions()}
        entry = infos[session.session_id]
        assert entry.tags == ["wing-probe"]
        assert entry.name is None  # 无名（首条消息未至）但已可寻址

    @pytest.mark.asyncio
    async def test_create_with_invalid_tag_fails_cleanly(self, file_sm: SessionManager):
        before = len(file_sm._sessions)
        with pytest.raises(ValueError):
            file_sm.create_session(tags=["ok", "bad tag"])
        assert len(file_sm._sessions) == before

    @pytest.mark.asyncio
    async def test_empty_tags_stay_off_disk(self, file_sm: SessionManager):
        """移除干净后 metadata.json 不带 tags 字段（存量文件零变化）。"""
        session = file_sm.create_session(tags=["a"])
        sid = session.session_id
        m = file_sm.set_session_tags(sid, remove=["a"])
        assert m.tags == []

        raw = json.loads((session.store.root / sid / "metadata.json").read_text())  # ty: ignore[unresolved-attribute]
        assert "tags" not in raw

    @pytest.mark.asyncio
    async def test_tags_survive_restart(self, file_sm: SessionManager, tmp_path: Path):
        session = file_sm.create_session(tags=["favorite"])
        _seed(session, "hello")
        sid = session.session_id

        restarted = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        infos = {i.id: i for i in restarted.list_sessions()}
        assert infos[sid].tags == ["favorite"]

        resumed = restarted.resume_session(sid)
        assert resumed.tags == ["favorite"]


class TestForkDoesNotInheritTags:
    @pytest.mark.asyncio
    async def test_fork_child_starts_untagged(self, file_sm: SessionManager):
        source = file_sm.create_session(tags=["favorite", "task=x"])
        _seed(source, "hello", "hi")

        last_uuid = source.context_manager.get_context_window()[-1].uuid
        child, _ = file_sm.fork_session(source.session_id, last_uuid)  # ty: ignore[invalid-argument-type, not-iterable]

        assert child.tags == []
        assert source.tags == ["favorite", "task=x"]


class TestStorePathSanitize:
    """读侧清洗与自愈：磁盘脏标签不阻断读取，下一次真实写盘落盘清洗结果。"""

    @pytest.mark.asyncio
    async def test_dirty_disk_tags_are_sanitized_and_healed(
        self, tmp_path: Path
    ) -> None:
        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})
        sid = "20250101-000000-abcdef04"
        session_dir = tmp_path / "sessions" / sid
        session_dir.mkdir(parents=True)
        (session_dir / "history.jsonl").write_text(
            '{"role": "user", "content": "hi", "uuid": "u1"}\n',
            encoding="utf-8",
        )
        # 手改 / 老数据：重复 + 违规格值（含逗号）
        (session_dir / "metadata.json").write_text(
            json.dumps({"tags": ["ok", "bad tag", "ok"]}), encoding="utf-8"
        )

        # 读：投影清洗，不写盘（磁盘原样）
        assert sm.set_session_tags(sid).tags == ["ok"]
        assert json.loads((session_dir / "metadata.json").read_text())["tags"] == [
            "ok",
            "bad tag",
            "ok",
        ]

        # 写：变更相对清洗后的集合计算；清洗结果随写盘自愈
        m = sm.set_session_tags(sid, add=["new"])
        assert m.tags == ["ok", "new"]
        assert m.added == ["new"]
        raw = json.loads((session_dir / "metadata.json").read_text())
        assert raw["tags"] == ["ok", "new"]
