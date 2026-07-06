"""Tests for ContextManager — 上下文管理。

核心场景：
1. add_message / add_messages: 基本消息管理
2. get_messages_for_llm: 返回活跃链 + system_prompt
3. compact: 压缩节点 + relink 行
4. rewind: 追加回退行，返回 draft
5. get_branch_targets: 活跃链上的 user 消息列表
6. extract_subchain + import_messages: fork 流程
"""

import json
import shutil
import tempfile
from pathlib import Path

import pytest

from wing.context_manager import ContextManager
from wing.common.tracked_list import TrackedList
from wing.schema import Message
from wing.session_manager import SessionManager


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp()
    yield Path(d)
    shutil.rmtree(d)


def _read_history(path: Path) -> list[dict]:
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


def _make_cm(tmp_dir: Path, session_id: str | None = None) -> ContextManager:
    """Create a ContextManager for testing under tmp_dir."""
    from wing.compactor import Compactor

    sid = session_id or "test-session"
    messages: TrackedList[Message] = TrackedList(tmp_dir / sid)
    return ContextManager(
        session_id=sid,
        messages=messages,
        system_prompt="You are a helpful assistant.",
        compactor=Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000),
    )


# ── 1. 基本消息管理 ──────────────────────────


class TestBasicMessageManagement:
    def test_add_message(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        msg = Message(role="user", content="hello")
        cm.add_message(msg)

        assert msg.uuid is not None
        assert len(cm._messages) == 1

    def test_add_messages(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        cm.add_messages(msgs)

        assert len(cm._messages) == 2
        assert msgs[1].parent_uuid == msgs[0].uuid

    @pytest.mark.asyncio
    async def test_get_messages_for_llm_includes_system_prompt(self, tmp_dir):
        from wing.openai_provider import OpenAIProvider

        cm = _make_cm(tmp_dir)
        cm.add_message(Message(role="user", content="hello"))

        prov = OpenAIProvider.__new__(OpenAIProvider)  # bare instance for type
        llm_msgs = await cm.get_messages_for_llm(model="test", model_provider=prov)
        assert len(llm_msgs) == 2  # system + user
        assert llm_msgs[0].role == "system"
        assert "helpful assistant" in (llm_msgs[0].content or "")
        assert llm_msgs[1].content == "hello"

    def test_get_context_window(self, tmp_dir):
        cm = _make_cm(tmp_dir)
        cm.add_message(Message(role="user", content="hello"))
        cm.add_message(Message(role="assistant", content="hi"))

        window = cm.get_context_window()
        assert len(window) == 2
        assert window[0].content == "hello"
        assert window[1].content == "hi"


# ── 2. Compact ────────────────────────────────


class TestCompact:
    def test_compact_writes_compact_node_and_relink_tail(self, tmp_dir):
        """compact 写入压缩节点 + relink tail 行到 JSONL。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="old1"),
            Message(role="assistant", content="old2"),
            Message(role="user", content="tail1"),
            Message(role="assistant", content="tail2"),
        ]
        cm.add_messages(msgs)

        # 手动触发 compact
        cut_idx = 2
        compact_node = Message(
            role="assistant",
            content="[Compact] summary of old1 and old2",
            parent_uuid=None,
            unzip_last_uuid=msgs[1].uuid,
        )
        compact_node.uuid = "compact-uuid"

        relink_tail = []
        prev_uuid = compact_node.uuid
        for i in range(cut_idx, len(msgs)):
            original = msgs[i]
            relinked = Message(
                role=original.role,
                content=original.content,
                parent_uuid=prev_uuid,
            )
            relinked.uuid = f"relink-{i}"
            relink_tail.append(relinked)
            prev_uuid = relinked.uuid

        cm._messages.append_detached(compact_node)
        for msg in relink_tail:
            cm._messages.append_detached(msg)
        cm._messages.set_tip(relink_tail[-1].uuid)

        entries = _read_history(tmp_dir / "test-session")
        assert len(entries) == 7  # 原始 4 + compact + 2 relink

        compact_entry = [e for e in entries if e.get("uuid") == "compact-uuid"][0]
        assert compact_entry["parent_uuid"] is None
        assert compact_entry["unzip_last_uuid"] == msgs[1].uuid

        relink1 = [e for e in entries if e.get("uuid") == "relink-2"][0]
        assert relink1["parent_uuid"] == "compact-uuid"
        assert relink1["content"] == "tail1"

    def test_compact_context_window(self, tmp_dir):
        """compact 后上下文窗口 = [compact_node, relink_tail...]。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="old1"),
            Message(role="assistant", content="old2"),
            Message(role="user", content="tail1"),
        ]
        cm.add_messages(msgs)

        compact_node = Message(
            role="assistant",
            content="[Compact] summary",
            parent_uuid=None,
            unzip_last_uuid=msgs[1].uuid,
        )
        compact_node.uuid = "cu"
        relinked = Message(role="user", content="tail1", parent_uuid="cu")
        relinked.uuid = "rt"

        cm._messages.append_detached(compact_node)
        cm._messages.append_detached(relinked)
        cm._messages.set_tip("rt")

        window = cm.get_context_window()
        assert len(window) == 2
        assert window[0].content == "[Compact] summary"
        assert window[1].content == "tail1"


# ── 3. Rewind ─────────────────────────────────


class TestRewind:
    def test_rewind_appends_line_and_returns_draft(self, tmp_dir):
        """rewind 追加回退行到 JSONL，返回 craft content 作为 draft。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
            Message(role="user", content="question"),
        ]
        cm.add_messages(msgs)

        target_uuid = msgs[2].uuid
        draft = cm.rewind(target_uuid)  # ty: ignore[invalid-argument-type]

        assert draft == "question"

        entries = _read_history(tmp_dir / "test-session")
        assert len(entries) == 4  # 原始 3 条 + 1 条回退行

        rewind_entry = entries[-1]
        assert rewind_entry["content"] == "hi"  # 复制了 parent 的内容
        assert rewind_entry["role"] == "assistant"
        assert rewind_entry["parent_uuid"] == msgs[0].uuid  # 祖父 uuid

    def test_rewind_context_window(self, tmp_dir):
        """rewind 后上下文窗口从回退行重建。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
            Message(role="user", content="question"),
        ]
        cm.add_messages(msgs)

        cm.rewind(msgs[2].uuid)  # ty: ignore[invalid-argument-type]

        window = cm.get_context_window()
        assert len(window) == 2
        assert window[0].content == "hello"
        assert window[1].content == "hi"

    def test_rewind_to_first_message(self, tmp_dir):
        """rewind 到第一条消息，上下文窗口只包含回退行。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        cm.add_messages(msgs)

        draft = cm.rewind(msgs[0].uuid)  # ty: ignore[invalid-argument-type]
        assert draft == "hello"

        window = cm.get_context_window()
        assert len(window) == 1
        assert window[0].parent_uuid is None

    def test_rewind_invalid_uuid(self, tmp_dir):
        """rewind 到不存在的 uuid 抛出 ValueError。"""
        cm = _make_cm(tmp_dir)
        cm.add_message(Message(role="user", content="hello"))
        with pytest.raises(ValueError):
            cm.rewind("nonexistent-uuid")

    def test_rewind_then_continue(self, tmp_dir):
        """rewind 后追加新消息，新消息从回退行继续。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
            Message(role="user", content="question"),
        ]
        cm.add_messages(msgs)

        cm.rewind(msgs[2].uuid)  # ty: ignore[invalid-argument-type]

        new_msg = Message(role="assistant", content="new answer")
        cm.add_message(new_msg)

        window = cm.get_context_window()
        assert len(window) == 3
        assert window[0].content == "hello"
        assert window[1].content == "hi"
        assert window[2].content == "new answer"

    def test_rewind_to_compressed_message(self, tmp_dir):
        """rewind 可以回退到被压缩的消息（通过 find 在 JSONL 中定位）。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="old1"),
            Message(role="assistant", content="old2"),
            Message(role="user", content="tail1"),
        ]
        cm.add_messages(msgs)

        # 模拟 compact
        compact_node = Message(
            role="assistant",
            content="[Compact]",
            parent_uuid=None,
            unzip_last_uuid=msgs[1].uuid,
        )
        compact_node.uuid = "cu"
        relinked = Message(role="user", content="tail1", parent_uuid="cu")
        relinked.uuid = "rt"
        cm._messages.append_detached(compact_node)
        cm._messages.append_detached(relinked)
        cm._messages.set_tip("rt")

        # rewind 到被压缩的 old1
        draft = cm.rewind(msgs[0].uuid)  # ty: ignore[invalid-argument-type]
        assert draft == "old1"

        # 上下文窗口只包含回退行（old1 是根消息）
        window = cm.get_context_window()
        assert len(window) == 1
        assert window[0].parent_uuid is None


# ── 4. get_branch_targets ─────────────────────


class TestGetBranchTargets:
    def test_branch_targets_from_active_chain(self, tmp_dir):
        """get_branch_targets 只返回活跃链上的 user 消息。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
            Message(role="user", content="question"),
        ]
        cm.add_messages(msgs)

        targets = cm.get_branch_targets()
        # 2 user messages in active chain + (current)
        assert len(targets) == 3
        user_targets = [t for t in targets if t["uuid"] != "current"]
        assert len(user_targets) == 2
        assert user_targets[0]["uuid"] == msgs[0].uuid
        assert user_targets[1]["uuid"] == msgs[2].uuid

    def test_branch_targets_after_rewind(self, tmp_dir):
        """rewind 后 get_branch_targets 只返回新活跃链上的 user 消息。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
            Message(role="user", content="question"),
        ]
        cm.add_messages(msgs)

        # rewind 到 question，活跃链变为 hello -> hi(rewind)
        cm.rewind(msgs[2].uuid)  # ty: ignore[invalid-argument-type]

        targets = cm.get_branch_targets()
        # 只有 hello 在活跃链上 + (current)
        assert len(targets) == 2
        user_targets = [t for t in targets if t["uuid"] != "current"]
        assert len(user_targets) == 1
        assert user_targets[0]["content"] == "hello"

    def test_branch_targets_after_compact(self, tmp_dir):
        """compact 后 get_branch_targets 包含压缩节点标记 + user 消息。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="old1"),
            Message(role="assistant", content="old2"),
            Message(role="user", content="tail1"),
        ]
        cm.add_messages(msgs)

        # 模拟 compact
        compact_node = Message(
            role="assistant",
            content="[Compact]",
            parent_uuid=None,
            unzip_last_uuid=msgs[1].uuid,
        )
        compact_node.uuid = "cu"
        relinked = Message(role="user", content="tail1", parent_uuid="cu")
        relinked.uuid = "rt"
        cm._messages.append_detached(compact_node)
        cm._messages.append_detached(relinked)
        cm._messages.set_tip("rt")

        targets = cm.get_branch_targets()
        # old1, [compact], tail1 + (current)
        assert len(targets) == 4
        # 第一个是 old1（完整链中的 user 消息）
        assert targets[0]["content"] == "old1"
        # 第二个是压缩节点标记
        assert targets[1]["uuid"] == "cu"
        assert targets[1]["content"].startswith("[Compact]")
        # 第三个是 tail1
        assert targets[2]["content"] == "tail1"
        # 最后是 (current)
        assert targets[3]["uuid"] == "current"

    def test_branch_targets_includes_current(self, tmp_dir):
        """get_branch_targets 末尾包含 (current) 选项。"""
        cm = _make_cm(tmp_dir)
        cm.add_message(Message(role="user", content="hello"))

        targets = cm.get_branch_targets()
        assert len(targets) == 2  # 1 user + (current)
        assert targets[-1]["uuid"] == "current"
        assert targets[-1]["content"] == "(current)"


# ── 5. Fork (extract_subchain + import_messages) ─


class TestFork:
    def test_extract_subchain_returns_subchain_and_draft(self, tmp_dir):
        """extract_subchain 返回 target 之前的子链 + draft。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
            Message(role="user", content="question"),
        ]
        cm.add_messages(msgs)

        subchain, draft = cm.extract_subchain(msgs[2].uuid)  # ty: ignore[invalid-argument-type]
        assert draft == "question"
        assert len(subchain) == 2
        assert subchain[0].content == "hello"
        assert subchain[1].content == "hi"

    def test_extract_subchain_to_first_message(self, tmp_dir):
        """extract_subchain 到第一条消息返回空子链。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        cm.add_messages(msgs)

        subchain, draft = cm.extract_subchain(msgs[0].uuid)  # ty: ignore[invalid-argument-type]
        assert draft == "hello"
        assert len(subchain) == 0

    def test_extract_subchain_current(self, tmp_dir):
        """extract_subchain 到 (current) 复制整个活跃链。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        cm.add_messages(msgs)

        subchain, draft = cm.extract_subchain("current")
        assert draft == ""
        assert len(subchain) == 2
        assert subchain[0].content == "hello"
        assert subchain[1].content == "hi"

    def test_extract_subchain_current_with_compact(self, tmp_dir):
        """extract_subchain('current') 返回完整链，包含压缩节点。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="old1"),
            Message(role="assistant", content="old2"),
            Message(role="user", content="tail1"),
        ]
        cm.add_messages(msgs)

        # 模拟 compact: 压缩 u1,u2 → compact(cu), relink tail1(rt)
        compact_node = Message(
            role="assistant",
            content="[Compact] summary",
            parent_uuid=None,
            unzip_last_uuid=msgs[1].uuid,
        )
        compact_node.uuid = "cu"
        relinked = Message(role="user", content="tail1", parent_uuid="cu")
        relinked.uuid = "rt"
        cm._messages.append_detached(compact_node)
        cm._messages.append_detached(relinked)
        cm._messages.set_tip("rt")

        # walk_full_chain: [old1(u1), old2(u2), compact(cu), tail1(rt)]
        subchain, draft = cm.extract_subchain("current")
        assert draft == ""
        assert len(subchain) == 4
        assert subchain[0].content == "old1"
        assert subchain[1].content == "old2"
        assert subchain[2].uuid == "cu"
        assert subchain[2].parent_uuid is None
        assert subchain[2].unzip_last_uuid == msgs[1].uuid
        assert subchain[3].uuid == "rt"
        assert subchain[3].parent_uuid == "cu"

    def test_fork_with_compact_preserves_topology(self, tmp_dir):
        """fork 后新 session 活跃链保留 compact 节点。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="old1"),
            Message(role="assistant", content="old2"),
            Message(role="user", content="tail1"),
        ]
        cm.add_messages(msgs)

        # 模拟 compact
        compact_node = Message(
            role="assistant",
            content="[Compact] summary",
            parent_uuid=None,
            unzip_last_uuid=msgs[1].uuid,
        )
        compact_node.uuid = "cu"
        relinked = Message(role="user", content="tail1", parent_uuid="cu")
        relinked.uuid = "rt"
        cm._messages.append_detached(compact_node)
        cm._messages.append_detached(relinked)
        cm._messages.set_tip("rt")

        # 记录原活跃链
        original_len = len(cm._messages.active_chain)

        # fork（extract_subchain + import_messages）
        subchain, _ = cm.extract_subchain("current")
        sm = SessionManager(sessions_path=tmp_dir)
        new_session_id = sm.import_messages(subchain)

        # 加载新 session
        new_path = sm._sessions_path / new_session_id
        new_tl = TrackedList.load(new_path, Message)

        # 新活跃链应与原活跃链长度一致（compact + tail1）
        new_active = new_tl.active_chain
        assert len(new_active) == original_len
        # 第一条是 compact 节点
        assert new_active[0].parent_uuid is None
        assert new_active[0].unzip_last_uuid is not None
        # 第二条的 parent 指向 compact
        assert new_active[1].parent_uuid == new_active[0].uuid
        assert new_active[1].content == "tail1"

        # history.jsonl 有 4 条（old1, old2, compact, tail1）
        entries = _read_history(new_path)
        assert len(entries) == 4

    def test_extract_subchain_to_compressed_message(self, tmp_dir):
        """extract_subchain 可以定位到被压缩的消息。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="old1"),
            Message(role="assistant", content="old2"),
            Message(role="user", content="tail1"),
        ]
        cm.add_messages(msgs)

        # 模拟 compact
        compact_node = Message(
            role="assistant",
            content="[Compact]",
            parent_uuid=None,
            unzip_last_uuid=msgs[1].uuid,
        )
        compact_node.uuid = "cu"
        relinked = Message(role="user", content="tail1", parent_uuid="cu")
        relinked.uuid = "rt"
        cm._messages.append_detached(compact_node)
        cm._messages.append_detached(relinked)
        cm._messages.set_tip("rt")

        # extract_subchain 到被压缩的 old2
        subchain, draft = cm.extract_subchain(msgs[1].uuid)  # ty: ignore[invalid-argument-type]
        assert draft == "old2"
        assert len(subchain) == 1
        assert subchain[0].content == "old1"

    def test_import_messages_creates_new_session(self, tmp_dir):
        """import_messages 创建新 session，重写 uuid，保持拓扑。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        cm.add_messages(msgs)

        subchain, _ = cm.extract_subchain(msgs[1].uuid)  # ty: ignore[invalid-argument-type]

        sm = SessionManager(sessions_path=tmp_dir)
        new_session_id = sm.import_messages(subchain, source_session_id=cm.id)
        assert new_session_id is not None
        assert new_session_id != cm.id

        new_path = sm._sessions_path / new_session_id
        entries = _read_history(new_path)
        assert len(entries) == 1
        assert entries[0]["content"] == "hello"
        assert entries[0]["parent_uuid"] is None

    def test_import_messages_rewrites_uuids(self, tmp_dir):
        """import_messages 重写所有 uuid，保持线性拓扑。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        cm.add_messages(msgs)

        subchain, _ = cm.extract_subchain("current")
        original_uuids = [m.uuid for m in subchain]

        sm = SessionManager(sessions_path=tmp_dir)
        new_session_id = sm.import_messages(subchain, source_session_id=cm.id)
        new_path = sm._sessions_path / new_session_id
        entries = _read_history(new_path)

        assert entries[0]["uuid"] != original_uuids[0]
        assert entries[1]["uuid"] != original_uuids[1]
        assert entries[0]["parent_uuid"] is None
        assert entries[1]["parent_uuid"] == entries[0]["uuid"]

    def test_import_messages_writes_metadata(self, tmp_dir):
        """import_messages 写入 metadata.json 记录 source_session_id。"""
        cm = _make_cm(tmp_dir)
        msgs = [
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        cm.add_messages(msgs)

        subchain, _ = cm.extract_subchain("current")
        sm = SessionManager(sessions_path=tmp_dir)
        new_session_id = sm.import_messages(subchain, source_session_id=cm.id)

        new_path = sm._sessions_path / new_session_id
        metadata_path = new_path / "metadata.json"
        assert metadata_path.exists()
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        assert metadata.get("forked_from") == cm.id
