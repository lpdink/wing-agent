"""会话标签（tags）单元测试。

覆盖：标签词汇校验与增删原语（``session/tags.py`` 纯函数）、**打标时间**
（``tag_meta``：加入记时间 / 移除删记录 / 幂等不刷新 / 读侧清洗）、
``SessionManager.set_session_tags`` 的两条路径（在内存 / 不在内存——后者
**不水合**、直接改磁盘 metadata）、创建即打标、列表投影、fork 不继承、
落盘洁癖（空标签与空记录不落字段、no-op 不产生写）。
"""

from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path

import pytest

from wing.schema import Message
from wing.session import SessionManager
from wing.session.tags import (
    MAX_TAG_LENGTH,
    MAX_TAGS_PER_SESSION,
    apply_tag_ops,
    sanitize_tag_meta,
    sanitize_tags,
    validate_tag,
)
from wing.store import FileSessionStore, SessionMetadata, TagMeta


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
            "c1\x9bcontrol",  # C1 CSI（8-bit 转义前导）
            "c1\x90control",  # C1 DCS
            "del\x7fchar",
            "nul\x00char",
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

    def test_drops_control_characters_including_c1(self):
        """C1 控制符（8-bit 转义前导）在读侧同样被清洗。"""
        assert sanitize_tags(["ok", "csi\x9bhere", "dcs\x90here", "del\x7f"]) == ["ok"]

    def test_applied_as_mutation_base(self):
        """apply_tag_ops 的既有集合先清洗：脏数据不阻断变更，且写出的结果干净。"""
        m = apply_tag_ops(["ok", "bad tag", "ok"], add=["new"])
        assert m.tags == ["ok", "new"]
        assert m.added == ["new"]


class TestTagTimes:
    """打标时间（``tag_meta``）：加入记时间、移除删记录、幂等 no-op 不刷新。"""

    def test_add_records_added_at_for_net_additions_only(self):
        m = apply_tag_ops(["old"], add=["pin", "old"], now="2026-10-05T21:30:12.000001")
        assert m.added == ["pin"]
        # 新标签带注入的时间；已有标签（无记录）不会被凭空补时间。
        assert set(m.tag_meta) == {"pin"}
        assert m.tag_meta["pin"].added_at == "2026-10-05T21:30:12.000001"

    def test_repeat_add_does_not_refresh_time(self):
        first = apply_tag_ops(None, add=["pin"], now="2026-10-05T21:30:12")
        again = apply_tag_ops(
            first.tags, add=["pin"], meta=first.tag_meta, now="2026-10-06T09:00:00"
        )
        assert again.added == []
        assert again.tag_meta["pin"].added_at == "2026-10-05T21:30:12"

    def test_remove_drops_the_record_and_readd_gets_a_fresh_time(self):
        first = apply_tag_ops(None, add=["pin"], now="2026-10-05T21:30:12")
        removed = apply_tag_ops(first.tags, remove=["pin"], meta=first.tag_meta)
        assert removed.tag_meta == {}

        readded = apply_tag_ops(
            removed.tags, add=["pin"], meta=removed.tag_meta, now="2026-10-08T08:00:00"
        )
        assert readded.tag_meta["pin"].added_at == "2026-10-08T08:00:00"

    def test_net_noop_keeps_the_record(self):
        """净 no-op（重复添加已存在的标签）：标签与记录都原样保留，不刷新时间。"""
        m = apply_tag_ops(
            ["pin"],
            add=["pin"],
            meta={"pin": {"added_at": "2026-10-05T21:30:12"}},
            now="2026-10-06T09:00:00",
        )
        assert not m.added and not m.removed
        assert m.tag_meta["pin"].added_at == "2026-10-05T21:30:12"

    def test_remove_wins_and_drops_the_record(self):
        """同一标签同时在两侧：remove 胜出（既有语义），记录同样删除。"""
        m = apply_tag_ops(
            ["pin"],
            add=["pin"],
            remove=["pin"],
            meta={"pin": {"added_at": "2026-10-05T21:30:12"}},
        )
        assert m.removed == ["pin"] and m.tags == []
        assert m.tag_meta == {}

    def test_default_time_is_local_iso(self):
        m = apply_tag_ops(None, add=["pin"])
        assert m.tag_meta["pin"].added_at is not None
        datetime.fromisoformat(m.tag_meta["pin"].added_at)  # 可解析即契约

    def test_sanitize_drops_unknown_keys_and_bad_values(self):
        clean = sanitize_tag_meta(
            {
                "pin": {"added_at": "2026-10-05T21:30:12"},
                "ghost": {"added_at": "x"},  # 不在标签集 → 丢弃
                "broken": 42,  # 值不是记录 → 丢弃
            },
            ["pin", "broken"],
        )
        assert clean == {"pin": TagMeta(added_at="2026-10-05T21:30:12")}

    def test_sanitize_never_raises_on_corrupt_shapes(self):
        assert sanitize_tag_meta(None, ["pin"]) == {}
        assert sanitize_tag_meta({"pin": None}, ["pin"]) == {}
        assert sanitize_tag_meta({"pin": {"added_at": 7}}, ["pin"]) == {}
        assert sanitize_tag_meta({"pin": ["not", "a", "record"]}, ["pin"]) == {}

    def test_sanitize_keeps_records_without_a_time(self):
        """手改 / 老版本可能留下无名记录的合法记录：标签有效，时间未知。"""
        clean = sanitize_tag_meta({"pin": {}}, ["pin"])
        assert clean == {"pin": TagMeta(added_at=None)}


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
    async def test_tag_times_ride_the_same_save_as_tags(self, file_sm: SessionManager):
        """打标时间与标签同一次落盘：加记时间、移除即消失（磁盘事实对账）。"""
        session = file_sm.create_session()
        _seed(session, "hello")
        sid = session.session_id

        m = file_sm.set_session_tags(sid, add=["pin"])
        assert m.tag_meta["pin"].added_at is not None
        meta = session.store.load_metadata(sid)
        assert meta is not None and meta.tag_meta is not None
        assert meta.tag_meta["pin"].added_at == m.tag_meta["pin"].added_at

        file_sm.set_session_tags(sid, remove=["pin"])
        meta = session.store.load_metadata(sid)
        assert meta is not None and not meta.tag_meta and not meta.tags

    @pytest.mark.asyncio
    async def test_tag_times_on_evicted_session_do_not_hydrate(
        self, file_sm: SessionManager
    ):
        """逐出会话打标：时间记录同样走 store 直写路径，不把会话换入内存。"""
        session = file_sm.create_session()
        _seed(session, "hello")
        sid = session.session_id
        del file_sm._sessions[sid]

        m = file_sm.set_session_tags(sid, add=["pin"])
        assert m.tag_meta["pin"].added_at is not None
        assert sid not in file_sm._sessions

        meta = session.store.load_metadata(sid)
        assert meta is not None and meta.tag_meta is not None
        assert meta.tag_meta["pin"].added_at == m.tag_meta["pin"].added_at

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
        self, file_sm: SessionManager, monkeypatch: pytest.MonkeyPatch
    ):
        """闸门：不合规 id 与"不存在"同价（LookupError），且不触碰文件系统。"""
        store = file_sm._stores["file"]
        reads: list[str] = []
        original = store.exists

        def spy(session_id: str) -> bool:
            reads.append(session_id)
            return original(session_id)

        monkeypatch.setattr(store, "exists", spy)

        for bad in (
            "../escape",
            "../../etc/passwd",
            "/tmp/absolute",
            ".media",  # 存储保留的点命名空间（媒体池）
            "..",
            "a..b",
            "line\nbreak",
            "\x7f",
            "",  # 空串
            "x" * 129,
        ):
            with pytest.raises(LookupError):
                file_sm.set_session_tags(bad, add=["x"])
        assert reads == [], f"闸门没有挡住: {reads}"

        # 任意安全字符串现在是**合法** id（编排方可自带），只是不存在——
        # 它会被交给 store 精确匹配，仍然 LookupError。
        with pytest.raises(LookupError):
            file_sm.set_session_tags("sid-1", add=["x"])
        assert reads == ["sid-1"]

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
        # store 路径写出的 tags-only 记录（未水合，故无 template/workspace 等）；
        # 打标时间随同一次写盘落下。
        raw = json.loads((session_dir / "metadata.json").read_text())
        assert raw["tags"] == ["favorite"]
        assert raw["tag_meta"]["favorite"]["added_at"] == (
            added.tag_meta["favorite"].added_at
        )

        removed = sm.set_session_tags(sid, remove=["favorite"])
        assert removed.tags == [] and removed.removed == ["favorite"]
        # 磁盘事实：真的清掉了（不是"端点说清掉了"）；重读不复活。
        assert sm.set_session_tags(sid).tags == []
        raw = json.loads((session_dir / "metadata.json").read_text())
        assert "tags" not in raw and "tag_meta" not in raw

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
    async def test_create_with_tags_records_creation_time(
        self, file_sm: SessionManager
    ):
        """创建即打标的时间 = 创建时刻（同一 ``apply_tag_ops`` 路径）。"""
        session = file_sm.create_session(tags=["pin"])
        _seed(session, "hello")

        meta = session.tag_meta
        assert meta["pin"].added_at is not None
        datetime.fromisoformat(meta["pin"].added_at)

        entry = {i.id: i for i in file_sm.list_sessions()}[session.session_id]
        assert entry.tag_meta["pin"].added_at == meta["pin"].added_at

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


class TestTagMetaProjection:
    """投影口径：``tag_meta`` 键集 ⊆ 清洗后的 ``tags``，损坏记录不阻断列表。"""

    def _session_dir(self, tmp_path: Path, sid: str) -> Path:
        d = tmp_path / "sessions" / sid
        d.mkdir(parents=True)
        (d / "history.jsonl").write_text(
            '{"role": "user", "content": "hi", "uuid": "u1"}\n', encoding="utf-8"
        )
        return d

    @pytest.mark.asyncio
    async def test_legacy_session_without_records_projects_empty_meta(
        self, tmp_path: Path
    ):
        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})
        sid = "20250101-000000-abcdef05"
        d = self._session_dir(tmp_path, sid)
        (d / "metadata.json").write_text(
            json.dumps({"tags": ["favorite"]}), encoding="utf-8"
        )

        entry = {i.id: i for i in sm.list_sessions()}[sid]
        assert entry.tags == ["favorite"]
        assert entry.tag_meta == {}  # 老数据无记录 = 时间未知，不影响标签本身

    @pytest.mark.asyncio
    async def test_stray_records_and_corrupt_values_never_reach_projection(
        self, tmp_path: Path
    ):
        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})
        sid = "20250101-000000-abcdef06"
        d = self._session_dir(tmp_path, sid)
        (d / "metadata.json").write_text(
            json.dumps(
                {
                    "tags": ["pin", "task=x"],
                    "tag_meta": {
                        "pin": {"added_at": "2026-10-05T21:30:12"},
                        "ghost": {"added_at": "2026-10-05T21:30:12"},  # 不在 tags
                        "task=x": "not-a-record",  # 坏值
                    },
                }
            ),
            encoding="utf-8",
        )

        entry = {i.id: i for i in sm.list_sessions()}[sid]
        assert entry.tags == ["pin", "task=x"]
        assert set(entry.tag_meta) == {"pin"}
        assert entry.tag_meta["pin"].added_at == "2026-10-05T21:30:12"


class TestListInclusionUsesSanitizedTags:
    """N-3：列表的"带标"判定与投影同口径（清洗后的集合）。"""

    @pytest.mark.asyncio
    async def test_garbage_only_tags_stay_hidden_valid_tags_listed(
        self, tmp_path: Path
    ) -> None:
        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})

        garbage = "20250101-000000-abcdef07"
        garbage_dir = tmp_path / "sessions" / garbage
        garbage_dir.mkdir(parents=True)
        (garbage_dir / "metadata.json").write_text(
            json.dumps({"tags": ["bad tag"]}), encoding="utf-8"
        )

        valid = "20250101-000000-abcdef08"
        valid_dir = tmp_path / "sessions" / valid
        valid_dir.mkdir(parents=True)
        (valid_dir / "metadata.json").write_text(
            json.dumps({"tags": ["ok"]}), encoding="utf-8"
        )

        ids = [i.id for i in sm.list_sessions()]
        assert valid in ids
        assert garbage not in ids  # 清洗后为空 = 无名无标
