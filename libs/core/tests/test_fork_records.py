"""fork 的记录前缀语义——子会话 = 源会话记录的前缀（append 顺序）。

核心不变量（产品语义："从任何一条 User Message fork，fork 出的会话也仍能
回到之前的任何 User Message"）：

- **记录**：子会话保留源会话在 fork 点之前的**全部**记录（含被压缩区间、
  含 rewind 留下的分叉），uuid 全量重映射且引用全部落在子记录集内部
  （不留悬空引用）——这是"回得去"的前提；
- **活跃链**：由 tip 沿 parent_uuid 回溯自然得出。压缩节点是活跃链的根
  （``parent_uuid=None`` + ``unzip_last_uuid``），被压缩区间**在记录里但不在
  链上**（不复活已摘要内容）；选压缩前的节点时压缩节点被切在前缀之外，子
  会话里压缩仿佛没发生过；
- **live == reload**：子会话的内存态经加载路径构造，与重启后加载一致；
- fork 候选（``get_branch_targets``）与源会话一致（含 ``[Compact]`` 标记与
  压缩前的 User Message）。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing.schema import Message
from wing.session_manager import SessionManager
from wing.store import FileSessionStore

# ============================================================
# fixtures / helpers
# ============================================================


@pytest.fixture
def root(tmp_path: Path) -> Path:
    return tmp_path / "sessions"


@pytest.fixture
def sm(root: Path) -> SessionManager:
    return SessionManager({"file": FileSessionStore(root)})


def _seed_compact(session) -> dict[str, str]:
    """在 session 上伪造一段"压缩后"的链，返回关键 uuid。

    形状（``history.jsonl`` 记录顺序）：

        old1(u1) → old2(a1) ─┬─ [Compact](c1, parent=None, unzip=a1)
                             └─ tail1(u2)
    """
    from wing.context import ContextManager

    cm: ContextManager = session.context_manager
    old1 = Message(role="user", content="old1")
    old2 = Message(role="assistant", content="old2")
    cm.add_message(old1)
    cm.add_message(old2)

    # append_detached 不填充 uuid/parent（调用方负责拓扑）——显式给定，便于断言
    compact_node = Message(
        role="assistant", content="[Compact] summary", parent_uuid=None
    )
    compact_node.uuid = "cu"
    compact_node.unzip_last_uuid = old2.uuid
    tail = Message(role="user", content="tail1", parent_uuid="cu")
    tail.uuid = "rt"
    cm._messages.append_detached(compact_node)
    cm._messages.append_detached(tail)
    cm._messages.set_tip("rt")

    old1_uuid, old2_uuid = old1.uuid, old2.uuid
    assert isinstance(old1_uuid, str) and isinstance(old2_uuid, str)
    return {"old1": old1_uuid, "old2": old2_uuid, "compact": "cu", "tail": "rt"}


def _contents(records: list[dict]) -> list[str]:
    """记录里的 content 序列（事件记录无 content → 空串占位）。"""
    return [record.get("content") or "" for record in records]


def _chain_contents(messages: list[Message]) -> list[str]:
    """上下文窗口（活跃链的 Message 投影）的 content 序列。"""
    return [message.content or "" for message in messages]


def _raw_metadata(root: Path, session_id: str) -> dict:
    return json.loads((root / session_id / "metadata.json").read_text(encoding="utf-8"))


def _records(root: Path, session_id: str) -> list[dict]:
    path = root / session_id / "history.jsonl"
    if not path.exists():  # 空子会话不创建日志文件
        return []
    return [
        json.loads(line)
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


# ============================================================
# 记录前缀
# ============================================================


class TestForkRecordPrefix:
    """子会话记录 = 源记录前缀（不含目标自身；draft 由客户端重发）。"""

    @pytest.mark.asyncio
    async def test_fork_at_message_copies_preceding_records(self, sm, root):
        session = sm.create_session()
        a = Message(role="user", content="alpha")
        b = Message(role="assistant", content="reply")
        c = Message(role="user", content="beta")
        session.context_manager.add_messages([a, b, c])

        assert c.uuid is not None
        result = sm.fork_session(session.session_id, c.uuid)
        assert result is not None
        child, draft = result
        assert draft == "beta"

        # 子记录 = 源记录 [a, b]（逐条语义等价、uuid 重映射）
        child_records = _records(root, child.session_id)
        assert _contents(child_records) == ["alpha", "reply"]
        assert [r["uuid"] for r in child_records] != [a.uuid, b.uuid]
        # 子会话活跃链 = [alpha, reply]，draft 由客户端重发
        assert _chain_contents(child.context_manager.get_context_window()) == [
            "alpha",
            "reply",
        ]

    @pytest.mark.asyncio
    async def test_fork_current_copies_all_records(self, sm, root):
        session = sm.create_session()
        session.context_manager.add_messages(
            [
                Message(role="user", content="alpha"),
                Message(role="assistant", content="reply"),
            ]
        )
        source_records = _records(root, session.session_id)

        result = sm.fork_session(session.session_id, "current")
        assert result is not None
        child, draft = result
        assert draft == ""

        child_records = _records(root, child.session_id)
        assert _contents(child_records) == _contents(source_records)
        assert len(child_records) == len(source_records)

    @pytest.mark.asyncio
    async def test_unknown_target_returns_none(self, sm):
        session = sm.create_session()
        assert sm.fork_session(session.session_id, "ghost") is None

    @pytest.mark.asyncio
    async def test_event_target_returns_none(self, sm, root):
        """事件记录不是对话节点：不能作为 fork 点（恢复旧契约）。"""
        session = sm.create_session()
        session.context_manager.add_message(Message(role="user", content="hi"))
        session.store.open_log(session.session_id).append(
            [
                {
                    "role": "event",
                    "type": "compact_done",
                    "uuid": "ev-1",
                    "parent_uuid": None,
                }
            ]
        )
        assert sm.fork_session(session.session_id, "ev-1") is None


class TestForkToolsRecord:
    """fork 记录的工具集必须与子会话**实际生效**的一致（含 ref 降级）。"""

    @pytest.mark.asyncio
    async def test_tools_record_matches_live_after_ref_degradation(
        self, sm, root, monkeypatch
    ):
        from wing.gateway.protocol import AgentOverride
        from wing.tool_registry import tool_registry

        session = sm.create_session(
            agent_override=AgentOverride(tools=["Read", "Glob"])
        )
        sid = session.session_id

        # 模拟远程工具宿主断连：注册表不再能解析该 ref（源会话 live 仍持有对象）
        real_resolve = tool_registry.resolve
        monkeypatch.setattr(
            tool_registry,
            "resolve",
            lambda ref: None if ref == "Glob" else real_resolve(ref),
        )

        result = sm.fork_session(sid, "current")
        assert result is not None
        child, _ = result
        # live 只剩可解析的；记录与 live 对齐（否则重启后声明集凭空变回 Glob）
        assert sorted(tool.name for tool in child.agent.tools) == ["Read"]
        assert _raw_metadata(root, child.session_id)["tools"] == ["Read"]

        restored = SessionManager({"file": FileSessionStore(root)}).resume_session(
            child.session_id
        )
        assert sorted(tool.name for tool in restored.agent.tools) == ["Read"]


# ============================================================
# 压缩：记录保留 + 活跃链不复活 + 回得去
# ============================================================


class TestForkAfterCompact:
    @pytest.mark.asyncio
    async def test_fork_current_keeps_region_but_chain_starts_at_compact(
        self, sm, root
    ):
        session = sm.create_session()
        _seed_compact(session)

        result = sm.fork_session(session.session_id, "current")
        assert result is not None
        child, _ = result
        child_records = _records(root, child.session_id)

        # 记录：被压缩区间随行走（old1 / old2 都在）
        assert _contents(child_records) == [
            "old1",
            "old2",
            "[Compact] summary",
            "tail1",
        ]
        # 活跃链：从压缩节点开始，已摘要内容不复活
        assert _chain_contents(child.context_manager.get_context_window()) == [
            "[Compact] summary",
            "tail1",
        ]
        # 压缩节点的 unzip 指向重映射后的区间末记录（子记录集内部，不悬空）
        compact_record = next(
            r for r in child_records if r.get("content") == "[Compact] summary"
        )
        child_uuids = {r["uuid"] for r in child_records}
        assert compact_record.get("parent_uuid") is None  # 根节点：落盘剥离 null 键
        assert compact_record["unzip_last_uuid"] in child_uuids

        # 回得去：fork 候选与源会话一致（含压缩前的 User Message 与 [Compact] 标记）
        targets = [t["content"] for t in child.context_manager.get_branch_targets()]
        # 压缩节点在候选列表里带 [Compact] 前缀（get_branch_targets 的展示格式）
        assert targets == ["old1", "[Compact] [Compact] summary", "tail1", "(current)"]

    @pytest.mark.asyncio
    async def test_fork_at_pre_compact_message_drops_the_compaction(self, sm, root):
        """选压缩前的节点：压缩节点是后来追加的记录 → 切在前缀之外。"""
        session = sm.create_session()
        ids = _seed_compact(session)

        result = sm.fork_session(session.session_id, ids["old1"])
        assert result is not None
        child, draft = result
        assert draft == "old1"

        # 子记录为空（old1 之前没有记录）；压缩仿佛没发生过
        assert _records(root, child.session_id) == []
        assert _chain_contents(child.context_manager.get_context_window()) == []

    @pytest.mark.asyncio
    async def test_fork_at_tail_keeps_region_as_records(self, sm, root):
        session = sm.create_session()
        ids = _seed_compact(session)

        result = sm.fork_session(session.session_id, ids["tail"])
        assert result is not None
        child, draft = result
        assert draft == "tail1"

        # 记录含被压缩区间；活跃链 = [compact]（tail1 不进链，由 draft 重发）
        child_records = _records(root, child.session_id)
        assert _contents(child_records) == ["old1", "old2", "[Compact] summary"]
        assert _chain_contents(child.context_manager.get_context_window()) == [
            "[Compact] summary"
        ]
        targets = [t["content"] for t in child.context_manager.get_branch_targets()]
        assert targets == ["old1", "[Compact] [Compact] summary", "(current)"]


# ============================================================
# 引用自洽 / live == reload
# ============================================================


class TestForkReferenceIntegrity:
    @pytest.mark.asyncio
    async def test_all_references_stay_inside_child_records(self, sm, root):
        session = sm.create_session()
        _seed_compact(session)

        result = sm.fork_session(session.session_id, "current")
        assert result is not None
        child, _ = result
        child_records = _records(root, child.session_id)
        child_uuids = {r["uuid"] for r in child_records}

        for record in child_records:
            for key in ("parent_uuid", "unzip_last_uuid"):
                value = record.get(key)
                assert value is None or value in child_uuids, (key, record)
        # 与源记录集无交集（全量重映射）
        source_uuids = {r["uuid"] for r in _records(root, session.session_id)}
        assert not (child_uuids & source_uuids)

    @pytest.mark.asyncio
    async def test_live_chain_equals_reloaded_chain(self, sm, root):
        session = sm.create_session()
        _seed_compact(session)
        result = sm.fork_session(session.session_id, "current")
        assert result is not None
        child, _ = result

        live = _chain_contents(child.context_manager.get_context_window())
        reloaded = SessionManager({"file": FileSessionStore(root)}).resume_session(
            child.session_id
        )
        assert _chain_contents(reloaded.context_manager.get_context_window()) == live
