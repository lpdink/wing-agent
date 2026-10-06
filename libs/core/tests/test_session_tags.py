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
    validate_tag,
)
from wing.store import FileSessionStore


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

    def test_remove_wins_when_tag_on_both_sides(self):
        """同一标签同时在 add / remove：先加后删，remove 胜出。"""
        m = apply_tag_ops(None, add=["x"], remove=["x"])
        assert m.tags == []
        assert m.added == ["x"]
        assert m.removed == ["x"]

    def test_read_only_call_returns_copy(self):
        m = apply_tag_ops(["a"])
        assert m.tags == ["a"]
        assert not m.added and not m.removed

    def test_cap_enforced_on_result(self):
        full = [f"t{i}" for i in range(MAX_TAGS_PER_SESSION)]
        with pytest.raises(ValueError):
            apply_tag_ops(full, add=["one-more"])

    def test_invalid_input_never_silently_dropped(self):
        with pytest.raises(ValueError):
            apply_tag_ops(None, add=["ok", "bad tag"])


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
        del file_sm._sessions[sid]  # ty: ignore[unresolved-attribute]

        m = file_sm.set_session_tags(sid, add=["favorite"])
        assert m.tags == ["favorite"]
        # 不水合：调用后会话仍在内存之外
        assert sid not in file_sm._sessions  # ty: ignore[unresolved-attribute]

        # 磁盘事实：metadata.json 里只有 tags 与必要字段（未创建会话对象）
        meta = session.store.load_metadata(sid)
        assert meta is not None and meta.tags == ["favorite"]
        assert meta.template_name is None and meta.workspace is None

        # 纯读同样不水合
        read = file_sm.set_session_tags(sid)
        assert read.tags == ["favorite"]
        assert sid not in file_sm._sessions  # ty: ignore[unresolved-attribute]

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
            file_sm.set_session_tags("20250101-000000-missing0", add=["x"])

    @pytest.mark.asyncio
    async def test_invalid_tag_raises_before_any_write(self, file_sm: SessionManager):
        session = file_sm.create_session()
        _seed(session, "hello")
        with pytest.raises(ValueError):
            file_sm.set_session_tags(session.session_id, add=["bad tag"])
        meta = session.store.load_metadata(session.session_id)
        assert meta is None or not meta.tags


class TestCreateAndProjection:
    @pytest.mark.asyncio
    async def test_create_with_tags(self, file_sm: SessionManager):
        session = file_sm.create_session(tags=["scheduler", "task=x"])
        _seed(session, "hello")  # 列表只收"有标题素材"的会话（既有语义）
        assert session.tags == ["scheduler", "task=x"]

        infos = {i.id: i for i in file_sm.list_sessions()}
        assert infos[session.session_id].tags == ["scheduler", "task=x"]

    @pytest.mark.asyncio
    async def test_create_with_invalid_tag_fails_cleanly(self, file_sm: SessionManager):
        before = len(file_sm._sessions)  # ty: ignore[unresolved-attribute]
        with pytest.raises(ValueError):
            file_sm.create_session(tags=["ok", "bad tag"])
        assert len(file_sm._sessions) == before  # ty: ignore[unresolved-attribute]

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
