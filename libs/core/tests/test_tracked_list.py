"""Tests for TrackedList — 持久化混合链容器。

核心场景：
1. append/extend: 基本追加，自动填充 uuid/parent_uuid（ChainNode 家族混排）
2. append_detached: 不自动填充，调用方自行管理拓扑
3. set_tip: 切换活跃链末尾
4. trace_chain: 从内存 map 沿 parent_uuid 回溯（倒序遍历）
5. find: 按 uuid 查找
6. load: 从 history.jsonl 恢复混合链（Message + 事件记录按 role 分发）
7. 混合链：事件节点与 Message 同链，rewind/fork 凭链序工作
"""

import json
import shutil
import tempfile
from pathlib import Path

import pytest
from pydantic import BaseModel
from pydantic.errors import PydanticUserError

from wing.common.tracked_list import TrackedList
from wing.event import DiffContentEvent
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
    """读遗留 newest.json（快照已废弃：新代码不写，仅存量文件存在）。"""
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
        # null 字段在存储边界被剥除（parent_uuid 为 None → 键不存在）
        assert "parent_uuid" not in entries[0]
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
        """ChainNode 家族约束：非 ChainNode 拒绝，家族内混排允许。"""
        tl = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Item(name="a"))
        with pytest.raises((TypeError, PydanticUserError)):
            tl.append(BaseModel())

    def test_no_newest_json_written(self, tmp_dir):
        """快照已废弃：append 不产生 newest.json。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        tl.append(Message(role="assistant", content="hi"))

        assert _read_newest(tmp_dir) == []
        assert not (tmp_dir / "newest.json").exists()

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

        # 内存对象保留 None（不自动填充）
        assert msg.uuid is None
        assert msg.parent_uuid is None
        # 存储记录剥除 null 键（uuid / parent_uuid 为 None → 键不存在）
        entries = _read_history(tmp_dir)
        assert len(entries) == 1
        assert "uuid" not in entries[0]
        assert "parent_uuid" not in entries[0]

    def test_append_detached_no_snapshot(self, tmp_dir):
        """append_detached 不产生快照文件。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        msg = Message(role="user", content="hello", uuid="u1", parent_uuid=None)
        tl.append_detached(msg)

        assert not (tmp_dir / "newest.json").exists()

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

    def test_set_tip_no_snapshot(self, tmp_dir):
        """set_tip 不产生快照文件（重放职责由 history.jsonl 混合日志承担）。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        m1 = Message(role="user", content="a", uuid="u1", parent_uuid=None)
        m2 = Message(role="assistant", content="b", uuid="u2", parent_uuid="u1")
        tl.append_detached(m1)
        tl.append_detached(m2)

        tl.set_tip("u1")
        assert not (tmp_dir / "newest.json").exists()

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
        """老 session（快照废弃前的日志）load 回归：仅凭 history.jsonl 恢复。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        tl.append(Message(role="assistant", content="hi"))

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        assert len(tl2) == 2
        assert tl2.active_chain[0].content == "hello"
        assert tl2.active_chain[1].content == "hi"

    def test_corrupt_newest_fallback(self, tmp_dir):
        """遗留 newest.json（损坏）不影响 load——快照不参与恢复。"""
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
        """load 后 find 从内存读取（不依赖外存）。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hello"))
        u = tl[0].uuid

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        found = tl2.find(u)  # ty: ignore[invalid-argument-type]
        assert found is not None
        assert found.content == "hello"

    def test_trace_chain_after_load_without_newest(self, tmp_dir):
        """load 后 trace_chain 从内存重建（不读外存）。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="a"))
        tl.append(Message(role="assistant", content="b"))

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

        found = tl.find(msg.uuid)  # ty: ignore[invalid-argument-type]
        assert found is not None
        assert found.content == "hello"

    def test_trace_chain_after_append_uses_memory(self, tmp_dir):
        """append 后 trace_chain 从内存读取。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="a"))
        tl.append(Message(role="assistant", content="b"))

        (tmp_dir / "history.jsonl").unlink()

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
        """ChainNode 家族约束（纯内存模式同构）：家族内混排 OK，族外拒绝。"""
        tl: TrackedList[ChainNode] = TrackedList()
        tl.append(Message(role="user", content="a"))
        # ChainNode 家族内混排：Item 也是 ChainNode，允许
        tl.append(Item(name="not a message"))
        # 非 ChainNode：拒绝
        with pytest.raises((TypeError, PydanticUserError)):
            tl.append(BaseModel())  # ty: ignore[invalid-argument-type]


# ── 8. 混合链（Message + 事件节点）─────────────


class TestMixedChain:
    """事件节点参与链构建：落盘、加载往返、rewind/fork 凭链序工作。"""

    def test_mixed_append_and_load_roundtrip(self, tmp_dir):
        """混合链 append → history.jsonl 记录序 → load 拓扑恢复。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir))
        u = Message(role="user", content="hi")
        tl.append(u)
        d = DiffContentEvent(path="a.txt", new_text="hello")
        tl.append(d)
        a = Message(role="assistant", content="ok")
        tl.append(a)
        t = Message(role="tool", tool_call_id="c1", content="res")
        tl.append(t)

        records = _read_history(tmp_dir)
        assert [r["role"] for r in records] == ["user", "event", "assistant", "tool"]
        ev = records[1]
        assert ev["type"] == "diff_content"
        assert "target" not in ev and "persist" not in ev
        assert ev["path"] == "a.txt"

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        chain = tl2.active_chain
        assert [type(x).__name__ for x in chain] == [
            "Message",
            "DiffContentEvent",
            "Message",
            "Message",
        ]
        # 拓扑连续性：事件节点在链上
        assert chain[1].parent_uuid == chain[0].uuid
        assert chain[2].parent_uuid == chain[1].uuid
        assert chain[3].parent_uuid == chain[2].uuid
        # 事件字段还原
        assert chain[1].path == "a.txt"

    def test_rewind_excludes_events_after_tip(self, tmp_dir):
        """rewind（set_tip）后，目标点之后的事件节点随链切换移出活跃链。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir))
        u1 = Message(role="user", content="q1")
        tl.append(u1)
        tl.append(DiffContentEvent(path="f1", new_text="x"))
        tl.append(Message(role="assistant", content="a1"))
        u2 = Message(role="user", content="q2")
        tl.append(u2)
        tl.append(DiffContentEvent(path="f2", new_text="y"))

        assert u2.uuid is not None
        tl.set_tip(u2.uuid)
        chain = tl.active_chain
        # tip=u2：f2 事件（u2 之后落盘）不在活跃链上；f1 保留
        assert [type(x).__name__ for x in chain] == [
            "Message",
            "DiffContentEvent",
            "Message",
            "Message",
        ]

    def test_fork_copies_events_with_prefix(self, tmp_dir):
        """fork（extend_detached 导入混合链前缀）事件随链拷贝。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="q1"))
        tl.append(DiffContentEvent(path="f1", new_text="x"))
        tl.append(Message(role="assistant", content="a1"))

        # fork：深拷贝混合链前缀（模拟 _remap_chain_uuids 后导入）
        from wing.session_manager import _remap_chain_uuids

        remapped = _remap_chain_uuids(tl.active_chain)
        forked: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir / "fork"))
        forked.extend_detached(remapped)

        kinds = [type(x).__name__ for x in forked.active_chain]
        assert kinds == ["Message", "DiffContentEvent", "Message"]
        # 拓扑重映射后保持连续
        fc = forked.active_chain
        assert fc[1].parent_uuid == fc[0].uuid
        assert fc[2].parent_uuid == fc[1].uuid

    def test_load_skips_unknown_event_type(self, tmp_dir):
        """未知事件 type 的记录被跳过（前向容忍），其余节点照常恢复。"""
        tl: TrackedList[Message] = TrackedList(FileMessageLog(tmp_dir))
        tl.append(Message(role="user", content="hi"))

        # 手工追加一条未知类型的 event 记录
        with open(tmp_dir / "history.jsonl", "a", encoding="utf-8") as f:
            f.write(
                json.dumps(
                    {
                        "role": "event",
                        "type": "from_the_future",
                        "uuid": "ev-1",
                        "parent_uuid": tl[0].uuid,
                    }
                )
                + "\n"
            )

        tl2 = TrackedList.load(FileMessageLog(tmp_dir), Message)
        assert len(tl2.active_chain) == 1
        assert tl2.active_chain[0].content == "hi"

    def test_persist_shrink_and_turn_result_disk_exclude(self, tmp_dir):
        """落盘集合收缩 + turn_result.result 字段级排除（经 sink 分流到存储边界）。

        - llm_call_metrics / tool_call_result：persist=False → 不落盘
          （孪生：截断审计由 Message.usage + Message.stop_reason 承载，工具结果
          由 role="tool" 的 Message 承载）；事件本身仍广播（走 event_bus）。
        - turn_result：persist=True 落盘，但 result 字段被 disk_exclude 剥离
          （最终 assistant 文本的孪生）；duration_ms / num_turns / usage /
          errors / subtype 保留——别处没有的审计字段。
        """
        from wing.agent.event_sink import AgentEventSink
        from wing.event_bus import event_bus
        from wing.schema import LLMUsage, ToolCall

        event_bus._subscribers.clear()
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir))
        sink = AgentEventSink(session_id="s", append_event=tl.append)
        try:
            sink.llm_metrics(
                LLMUsage(
                    prompt_tokens=100,
                    completion_tokens=50,
                    cached_tokens=10,
                    first_chunk_rt_ms=123.0,
                    tokens_per_sec=42.0,
                    stop_reason="max_tokens",
                )
            )
            sink.tool_finished(
                ToolCall(id="tc", name="Bash", arguments={}),
                result="ok",
                success=True,
                model="m",
            )
            sink.turn_result(
                subtype="success",
                result="final assistant text",
                num_turns=2,
                duration_ms=999,
                usage={"input_tokens": 10, "output_tokens": 5},
            )
        finally:
            event_bus._subscribers.clear()

        records = _read_history(tmp_dir)
        types = [r.get("type") for r in records]
        # 孪生事件不落盘
        assert "llm_call_metrics" not in types
        assert "tool_call_result" not in types
        assert "tool_result_turn" not in types
        # turn_result 落盘但不含 result（disk_exclude），审计字段保留
        (tr,) = [r for r in records if r.get("type") == "turn_result"]
        assert "result" not in tr
        assert tr["subtype"] == "success"
        assert tr["num_turns"] == 2
        assert tr["duration_ms"] == 999
        assert tr["usage"] == {"input_tokens": 10, "output_tokens": 5}

    def test_turn_result_wire_frame_keeps_result(self, tmp_dir):
        """对照：turn_result 的 wire 帧仍携带 result（stdio 前端消费）。

        disk_exclude 只作用于存储记录，不影响直播/重放帧。
        """
        from wing.event import TurnResultEvent, wire_dump

        ev = TurnResultEvent(result="final text", num_turns=1, duration_ms=10)
        frame = wire_dump(ev)
        assert frame["result"] == "final text"
        # wire 帧剥除存储专用字段
        assert "parent_uuid" not in frame
        assert "unzip_last_uuid" not in frame
        assert "role" not in frame
        assert "persist" not in frame
        assert frame["uuid"]  # uuid 保留
