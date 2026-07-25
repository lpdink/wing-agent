"""Tests for TrackedList — 持久化链容器。

核心场景：
1. append/extend: 基本追加，自动填充 uuid/parent_uuid
2. append_detached: 不自动填充，调用方自行管理拓扑
3. set_tip: 切换活跃链末尾
4. trace_chain: 从 JSONL 沿 parent_uuid 回溯（倒序遍历）
5. find: 按 uuid 查找
6. load: 从 newest.json 或 history.jsonl 恢复
"""

import json
import shutil
import tempfile
from pathlib import Path

import pytest
from pydantic import BaseModel
from pydantic.errors import PydanticUserError

from wing.common.tracked_list import TrackedList
from wing.store import FileMessageLog
from wing.schema import ChainNode, Message


class Item(ChainNode):
    """Simple pydantic model for testing TrackedList type locking."""

    name: str
    value: int = 0


@pytest.fixture
def tmp_dir():
    """Provide a temporary directory for TrackedList persistence."""
    d = tempfile.mkdtemp()
    yield Path(d)
    shutil.rmtree(d)


def _read_history(path: Path) -> list[dict]:
    """Read all lines from history.jsonl as parsed JSON dicts."""
    hist = path / "history.jsonl"
    if not hist.exists():
        return []
    lines = []
    with open(hist, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line:
                lines.append(json.loads(line))
    return lines


def _read_newest(path: Path) -> list[dict]:
    """Read newest.json as parsed JSON."""
    newest = path / "newest.json"
    if not newest.exists():
        return []
    return json.loads(newest.read_text(encoding="utf-8"))


# ── 1. append/extend ──────────────────────────


class TestAppendExtend:
    def test_append_assigns_uuid(self, tmp_dir):
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg = Message(role="user", content="hello")
        tl.append(msg)

        assert msg.uuid is not None
        assert msg.parent_uuid is None  # first message
        assert len(tl) == 1

        entries = _read_history(tmp_dir)
        assert len(entries) == 1
        assert entries[0]["uuid"] == msg.uuid
        assert entries[0]["parent_uuid"] is None
        assert entries[0]["role"] == "user"
        assert entries[0]["content"] == "hello"
        assert "ts" in entries[0]

    def test_append_chain_parent(self, tmp_dir):
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg1 = Message(role="user", content="hello")
        msg2 = Message(role="assistant", content="hi")
        tl.append(msg1)
        tl.append(msg2)

        assert msg1.parent_uuid is None
        assert msg2.parent_uuid == msg1.uuid

        entries = _read_history(tmp_dir)
        assert len(entries) == 2
        assert entries[1]["parent_uuid"] == msg1.uuid

    def test_extend_assigns_uuids(self, tmp_dir):
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
            Message(role="user", content="bye"),
        ]
        tl.extend(msgs)

        assert len(tl) == 3
        assert msgs[0].uuid is not None
        assert msgs[0].parent_uuid is None
        assert msgs[1].parent_uuid == msgs[0].uuid
        assert msgs[2].parent_uuid == msgs[1].uuid

        entries = _read_history(tmp_dir)
        assert len(entries) == 3

    def test_type_locking(self, tmp_dir):
        tl = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Item(name="a"))
        with pytest.raises((TypeError, PydanticUserError)):
            tl.append(BaseModel())

    def test_newest_json_updated(self, tmp_dir):
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        newest = _read_newest(tmp_dir)
        assert len(newest) == 1
        assert newest[0]["role"] == "user"

    def test_no_snapshot_lines(self, tmp_dir):
        """Verify that append does NOT write snapshot lines."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        tl.append(Message(role="assistant", content="hi"))

        entries = _read_history(tmp_dir)
        for entry in entries:
            assert entry.get("op") != "snapshot"

    def test_active_chain_matches_data(self, tmp_dir):
        """active_chain property returns current _data."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        tl.append(Message(role="assistant", content="hi"))

        chain = tl.active_chain
        assert len(chain) == 2
        assert chain[0].content == "hello"
        assert chain[1].content == "hi"

    def test_last_uuid_property(self, tmp_dir):
        """last_uuid returns the uuid of the last message."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg1 = Message(role="user", content="hello")
        msg2 = Message(role="assistant", content="hi")
        tl.append(msg1)
        tl.append(msg2)

        assert tl.last_uuid == msg2.uuid


# ── 2. append_detached ────────────────────────


class TestAppendDetached:
    def test_append_detached_writes_as_is(self, tmp_dir):
        """append_detached writes to JSONL without auto-filling uuid/parent_uuid."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg = Message(
            role="user",
            content="hello",
            uuid="my-custom-uuid",
            parent_uuid="some-parent",
        )
        tl.append_detached(msg)

        entries = _read_history(tmp_dir)
        assert len(entries) == 1
        assert entries[0]["uuid"] == "my-custom-uuid"
        assert entries[0]["parent_uuid"] == "some-parent"

        assert len(tl.active_chain) == 1
        assert tl.last_uuid == "my-custom-uuid"

    def test_append_detached_preserves_none_uuid(self, tmp_dir):
        """append_detached preserves None uuid (doesn't auto-fill)."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg = Message(role="user", content="hello", uuid=None, parent_uuid=None)
        tl.append_detached(msg)

        entries = _read_history(tmp_dir)
        assert len(entries) == 1
        assert entries[0]["uuid"] is None
        assert entries[0]["parent_uuid"] is None

    def test_append_detached_updates_newest(self, tmp_dir):
        """append_detached updates newest.json."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg = Message(role="user", content="hello", uuid="u1", parent_uuid=None)
        tl.append_detached(msg)

        newest = _read_newest(tmp_dir)
        assert len(newest) == 1
        assert newest[0]["uuid"] == "u1"

    def test_append_detached_chain(self, tmp_dir):
        """Multiple append_detached calls build a chain."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        assert len(tl.active_chain) == 3
        assert tl.last_uuid == "u3"
        assert [m.content for m in tl.active_chain] == ["a", "b", "c"]


# ── 3. set_tip ────────────────────────────────


class TestSetTip:
    def test_set_tip_switches_active_chain(self, tmp_dir):
        """set_tip rebuilds active chain from the specified tip."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        tl.set_tip("u2")
        assert len(tl.active_chain) == 2
        assert tl.active_chain[0].content == "a"
        assert tl.active_chain[1].content == "b"
        assert tl.last_uuid == "u2"

    def test_set_tip_to_root(self, tmp_dir):
        """set_tip to root gives chain of length 1."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        tl.append_detached(m1)
        tl.append_detached(m2)

        tl.set_tip("u1")
        assert len(tl.active_chain) == 1
        assert tl.active_chain[0].content == "a"
        assert tl.last_uuid == "u1"

    def test_set_tip_updates_newest(self, tmp_dir):
        """set_tip updates newest.json cache."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        tl.append_detached(m1)
        tl.append_detached(m2)

        tl.set_tip("u1")
        newest = _read_newest(tmp_dir)
        assert len(newest) == 1
        assert newest[0]["uuid"] == "u1"

    def test_set_tip_invalid_uuid(self, tmp_dir):
        """set_tip with nonexistent uuid raises ValueError."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        with pytest.raises(ValueError):
            tl.set_tip("nonexistent-uuid")


# ── 4. trace_chain ────────────────────────────


class TestTraceChain:
    def test_trace_chain_from_leaf(self, tmp_dir):
        """trace_chain from leaf traverses to root (倒序遍历)."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        chain = tl.trace_chain()
        assert len(chain) == 3
        assert [m.content for m in chain] == ["a", "b", "c"]

    def test_trace_chain_from_uuid(self, tmp_dir):
        """trace_chain from specific uuid."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        chain = tl.trace_chain(from_uuid="u2")
        assert len(chain) == 2
        assert [m.content for m in chain] == ["a", "b"]

    def test_trace_chain_empty(self, tmp_dir):
        """trace_chain on empty JSONL returns empty list."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        chain = tl.trace_chain()
        assert chain == []

    def test_trace_chain_single(self, tmp_dir):
        """trace_chain with single message."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        tl.append_detached(m1)

        chain = tl.trace_chain()
        assert len(chain) == 1
        assert chain[0].content == "a"

    def test_trace_chain_returns_typed_objects(self, tmp_dir):
        """trace_chain returns typed Message objects, not raw dicts."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="hello", uuid="u1", parent_uuid=None)
        tl.append_detached(m1)

        chain = tl.trace_chain()
        assert len(chain) == 1
        msg = chain[0]
        assert isinstance(msg, Message)
        assert msg.role == "user"
        assert msg.content == "hello"
        assert msg.uuid == "u1"
        assert msg.parent_uuid is None

    def test_trace_chain_branch_not_included(self, tmp_dir):
        """trace_chain only follows the specified branch (倒序遍历)."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        # Branch: u1 -> u4 (rewind scenario)
        m4 = Message(role="assistant", content="b2", uuid="u4", parent_uuid="u1")
        tl.append_detached(m4)

        # trace_chain from leaf picks the last leaf (u4, the branch)
        chain = tl.trace_chain()
        assert len(chain) == 2
        assert [m.content for m in chain] == ["a", "b2"]

        # trace_chain from u3: u1 -> u2 -> u3 (original chain)
        chain = tl.trace_chain(from_uuid="u3")
        assert len(chain) == 3
        assert [m.content for m in chain] == ["a", "b", "c"]

    def test_trace_chain_stops_at_compact_node(self, tmp_dir):
        """trace_chain stops at compact node (parent_uuid=None)."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="old1", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="old2", uuid="u2", parent_uuid="u1")
        tl.append_detached(m1)
        tl.append_detached(m2)

        # Compact node: parent_uuid=None, unzip_last_uuid=u2
        compact = Message(
            role="assistant",
            content="[Compact]",
            uuid="cu",
            parent_uuid=None,
            unzip_last_uuid="u2",
        )
        tl.append_detached(compact)

        # trace_chain from compact node: just [compact]
        chain = tl.trace_chain(from_uuid="cu")
        assert len(chain) == 1
        assert chain[0].uuid == "cu"


# ── 4b. trace_full_chain ──────────────────────


class TestTraceFullChain:
    """trace_full_chain 跳过压缩节点，walk_full_chain 包含压缩节点。"""

    def test_trace_full_chain_no_compact(self, tmp_dir):
        """无压缩时 trace_full_chain 与 trace_chain 结果相同。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        full = tl.trace_full_chain()
        active = tl.trace_chain()
        assert full == active
        assert [m.content for m in full] == ["a", "b", "c"]

    def test_trace_full_chain_skips_compact(self, tmp_dir):
        """trace_full_chain 跳过压缩节点，沿 unzip_last_uuid 继续。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="old1", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="old2", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="tail1", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        # Compact: 压缩 u1,u2 → compact, relink u3
        compact = Message(
            role="assistant",
            content="[Compact] summary",
            uuid="cu",
            parent_uuid=None,
            unzip_last_uuid="u2",
        )
        relinked = Message(role="user", content="tail1", uuid="rt", parent_uuid="cu")
        tl.append_detached(compact)
        tl.append_detached(relinked)
        tl.set_tip("rt")

        # trace_chain: [compact, relinked]
        active = tl.trace_chain()
        assert [m.content for m in active] == ["[Compact] summary", "tail1"]

        # trace_full_chain: [old1, old2, tail1] — 跳过 compact
        full = tl.trace_full_chain()
        assert [m.content for m in full] == ["old1", "old2", "tail1"]

    def test_trace_full_chain_from_uuid(self, tmp_dir):
        """trace_full_chain 从指定 uuid 开始。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        chain = tl.trace_full_chain(from_uuid="u2")
        assert [m.content for m in chain] == ["a", "b"]

    def test_trace_full_chain_empty(self, tmp_dir):
        """trace_full_chain on empty returns empty list."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        assert tl.trace_full_chain() == []

    def test_walk_full_chain_includes_compact(self, tmp_dir):
        """walk_full_chain 包含压缩节点。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="old1", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="old2", uuid="u2", parent_uuid="u1")
        m3 = Message(role="user", content="tail1", uuid="u3", parent_uuid="u2")
        tl.append_detached(m1)
        tl.append_detached(m2)
        tl.append_detached(m3)

        compact = Message(
            role="assistant",
            content="[Compact] summary",
            uuid="cu",
            parent_uuid=None,
            unzip_last_uuid="u2",
        )
        relinked = Message(role="user", content="tail1", uuid="rt", parent_uuid="cu")
        tl.append_detached(compact)
        tl.append_detached(relinked)
        tl.set_tip("rt")

        # walk_full_chain: [old1, old2, compact, tail1] — 包含 compact
        walked = tl.walk_full_chain()
        assert [m.content for m in walked] == [
            "old1",
            "old2",
            "[Compact] summary",
            "tail1",
        ]
        # compact 节点可识别
        compact_in_result = [m for m in walked if m.unzip_last_uuid is not None]
        assert len(compact_in_result) == 1
        assert compact_in_result[0].uuid == "cu"


# ── 5. find ───────────────────────────────────


class TestFind:
    def test_find_by_uuid(self, tmp_dir):
        """find returns typed Message by uuid."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        tl.append_detached(m1)
        tl.append_detached(m2)

        found = tl.find("u1")
        assert found is not None
        assert isinstance(found, Message)
        assert found.content == "a"
        assert found.uuid == "u1"
        assert found.role == "user"

    def test_find_nonexistent(self, tmp_dir):
        """find returns None for nonexistent uuid."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        assert tl.find("nonexistent") is None

    def test_find_last_write_wins(self, tmp_dir):
        """find returns the last occurrence of a uuid (last-write-wins)."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="original", uuid="u1", parent_uuid=None)
        tl.append_detached(m1)

        m2 = Message(role="user", content="rewritten", uuid="u1", parent_uuid=None)
        tl.append_detached(m2)

        found = tl.find("u1")
        assert found is not None
        assert found.content == "rewritten"


# ── 6. load ───────────────────────────────────


class TestLoad:
    def test_normal_load(self, tmp_dir):
        """Load from newest.json when it exists and is valid."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        tl.append(Message(role="assistant", content="hi"))

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        assert len(tl2) == 2
        assert tl2.active_chain[0].content == "hello"
        assert tl2.active_chain[1].content == "hi"
        assert tl2.last_uuid == tl2.active_chain[1].uuid

    def test_missing_newest_fallback(self, tmp_dir):
        """newest.json missing → fallback to history.jsonl."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        tl.append(Message(role="assistant", content="hi"))

        (tmp_dir / "newest.json").unlink()

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        assert len(tl2) == 2
        assert tl2.active_chain[0].content == "hello"
        assert tl2.active_chain[1].content == "hi"

    def test_corrupt_newest_fallback(self, tmp_dir):
        """newest.json corrupt → fallback to history.jsonl."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))

        (tmp_dir / "newest.json").write_text("NOT JSON!!")

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        assert len(tl2) == 1
        assert tl2.active_chain[0].content == "hello"

    def test_last_uuid_recovery(self, tmp_dir):
        """_last_uuid is restored from the loaded chain."""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg1 = Message(role="user", content="hello")
        msg2 = Message(role="assistant", content="hi")
        tl.append(msg1)
        tl.append(msg2)

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        assert tl2.last_uuid == msg2.uuid

        msg3 = Message(role="user", content="bye")
        tl2.append(msg3)
        assert msg3.parent_uuid == msg2.uuid

    def test_load_empty_dir(self, tmp_dir):
        """Load from empty directory returns empty TrackedList."""
        tl = TrackedList.load(FileMessageLog(tmp_dir), Message)
        assert len(tl) == 0
        assert tl.last_uuid is None


# ── 7. In-memory full chain ───────────────────


class TestInMemoryCache:
    """验证 find/trace_chain 从内存读取，而非每次读外存。"""

    def test_find_after_load_without_newest(self, tmp_dir):
        """删除 newest.json 后 load，find 仍能从内存中找到消息。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        u = tl[0].uuid
        (tmp_dir / "newest.json").unlink()

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        found = tl2.find(u)  # ty: ignore[invalid-argument-type]
        assert found is not None
        assert found.content == "hello"

    def test_trace_chain_after_load_without_newest(self, tmp_dir):
        """删除 newest.json 后 load，trace_chain 从内存重建。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="a"))
        tl.append(Message(role="assistant", content="b"))
        (tmp_dir / "newest.json").unlink()

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        chain = tl2.trace_chain()
        assert len(chain) == 2
        assert chain[0].content == "a"
        assert chain[1].content == "b"

    def test_all_items_includes_branches(self, tmp_dir):
        """内存 map 包含分支消息（不在活跃链中也能 find）。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append_detached(
            Message(role="user", content="a", uuid="u1", parent_uuid=None)
        )
        tl.append_detached(
            Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        )
        tl.append_detached(
            Message(role="assistant", content="b2", uuid="u3", parent_uuid="u1")
        )

        # 活跃链是 u1 -> u3（最后叶节点），但 u2 在内存中
        chain = tl.trace_chain()
        assert [m.content for m in chain] == ["a", "b2"]

        # find u2 应该成功（在内存中）
        found = tl.find("u2")
        assert found is not None
        assert found.content == "b"

    def test_find_after_append_uses_memory(self, tmp_dir):
        """append 后立即 find，从内存读取（不依赖外存）。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg = Message(role="user", content="hello")
        tl.append(msg)

        # 删除外存文件，验证 find 仍能工作
        (tmp_dir / "history.jsonl").unlink()
        (tmp_dir / "newest.json").unlink()

        found = tl.find(msg.uuid)  # ty: ignore[invalid-argument-type]
        assert found is not None
        assert found.content == "hello"

    def test_trace_chain_after_append_uses_memory(self, tmp_dir):
        """append 后 trace_chain 从内存读取。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="a"))
        tl.append(Message(role="assistant", content="b"))

        (tmp_dir / "history.jsonl").unlink()
        (tmp_dir / "newest.json").unlink()

        chain = tl.trace_chain()
        assert len(chain) == 2
        assert chain[0].content == "a"
        assert chain[1].content == "b"

    def test_extend_updates_memory(self, tmp_dir):
        """extend 后 find 和 trace_chain 从内存读取。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msgs = [
            Message(role="user", content="a"),
            Message(role="assistant", content="b"),
        ]
        tl.extend(msgs)

        (tmp_dir / "history.jsonl").unlink()
        (tmp_dir / "newest.json").unlink()

        found = tl.find(msgs[0].uuid)  # ty: ignore[invalid-argument-type]
        assert found is not None
        assert found.content == "a"

        chain = tl.trace_chain()
        assert len(chain) == 2

    def test_append_detached_updates_memory(self, tmp_dir):
        """append_detached 后 find 从内存读取。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg = Message(role="user", content="hello", uuid="my-uuid", parent_uuid=None)
        tl.append_detached(msg)

        (tmp_dir / "history.jsonl").unlink()
        (tmp_dir / "newest.json").unlink()

        found = tl.find("my-uuid")
        assert found is not None
        assert found.content == "hello"

    def test_set_tip_uses_memory(self, tmp_dir):
        """set_tip 从内存重建链（不读外存）。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append_detached(
            Message(role="user", content="a", uuid="u1", parent_uuid=None)
        )
        tl.append_detached(
            Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        )
        tl.append_detached(
            Message(role="user", content="c", uuid="u3", parent_uuid="u2")
        )

        (tmp_dir / "history.jsonl").unlink()
        (tmp_dir / "newest.json").unlink()

        tl.set_tip("u2")
        assert len(tl.active_chain) == 2
        assert tl.active_chain[0].content == "a"
        assert tl.active_chain[1].content == "b"

    def test_last_write_wins_in_memory(self, tmp_dir):
        """相同 uuid 的消息，后写入的覆盖先写入的（last-write-wins）。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append_detached(
            Message(role="user", content="original", uuid="u1", parent_uuid=None)
        )
        tl.append_detached(
            Message(role="user", content="rewritten", uuid="u1", parent_uuid=None)
        )

        (tmp_dir / "history.jsonl").unlink()
        (tmp_dir / "newest.json").unlink()

        found = tl.find("u1")
        assert found is not None
        assert found.content == "rewritten"


class TestPureMemoryMode:
    """log=None：纯内存链引擎，全部操作无文件产生。"""

    def test_full_chain_ops_no_files(self, tmp_dir):
        tl: TrackedList[Message] = TrackedList()
        tl.append(Message(role="user", content="a"))
        tl.append(Message(role="assistant", content="b"))
        tl.extend([Message(role="user", content="c")])

        assert len(tl) == 3
        assert tl.active_chain[-1].content == "c"
        assert tl.last_uuid is not None
        assert tl.find(tl.last_uuid) is not None
        assert len(tl.trace_chain()) == 3
        assert len(tl.walk_full_chain()) == 3

        # rewind 风格：append_detached + set_tip
        branch = Message(role="user", content="branch", parent_uuid=None)
        branch.uuid = "branch-1"
        tl.append_detached(branch)
        tl.set_tip("branch-1")
        assert tl.active_chain == [branch]

        # aux NOP
        tl.write_aux("pending_compact", {"k": "v"})
        assert tl.read_aux("pending_compact") is None
        tl.delete_aux("pending_compact")

        # 目录始终为空（无任何文件产生）
        assert list(Path(tmp_dir).iterdir()) == []

    def test_type_locking_still_works(self, tmp_dir):
        tl: TrackedList[Message] = TrackedList()
        tl.append(Message(role="user", content="a"))
        with pytest.raises(TypeError):
            tl.append(Item(name="not a message"))  # ty: ignore[invalid-argument-type]
