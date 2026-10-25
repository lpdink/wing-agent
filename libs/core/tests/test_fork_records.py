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

from wing.schema import LLMUsage, Message
from wing.session import SessionManager
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
    compact_node.usage_boundary_uuid = "rt"  # 换入时刻的链尖 = tail1
    tail = Message(role="user", content="tail1", parent_uuid="cu")
    tail.uuid = "rt"
    cm._messages.append_detached(compact_node)
    cm._messages.append_detached(tail)
    cm._messages.set_tip("rt")

    old1_uuid, old2_uuid = old1.uuid, old2.uuid
    assert isinstance(old1_uuid, str) and isinstance(old2_uuid, str)
    return {"old1": old1_uuid, "old2": old2_uuid, "compact": "cu", "tail": "rt"}


def _contents(records: list[dict]) -> list[str]:
    """记录里的 content 序列（事件记录无 content → 空串占位）。

    assistant 记录只存 content_blocks，content 经 ``Message`` 派生——与消费方
    （载入路径 / 前端投影）读到的值一致。
    """
    return [
        (
            ""
            if record.get("role") == "event"
            else Message.model_validate(record).content
        )
        or ""
        for record in records
    ]


def _record_with_content(records: list[dict], text: str) -> dict:
    """按派生 content 找 Message 记录（事件记录跳过）。"""
    for record in records:
        if record.get("role") == "event":
            continue
        if Message.model_validate(record).content == text:
            return record
    raise AssertionError(f"no message record with content {text!r}")


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

    def test_fork_slice_stops_reading_at_target(self):
        """命中目标即停：fork 点之后的记录不读也不解析（流式读的头条收益）。"""
        from wing.session.manager import _fork_slice

        consumed: list[str] = []

        def records():
            for i in range(10):
                consumed.append(f"u{i}")
                yield {"uuid": f"u{i}", "role": "user", "content": f"m{i}"}

        prefix, draft = _fork_slice(records(), "u2")

        assert consumed == ["u0", "u1", "u2"]
        assert [r["content"] for r in prefix] == ["m0", "m1"]
        assert draft == "m2"


class TestForkToolsRecord:
    """fork 记录的工具集必须与子会话**实际生效**的一致（含 ref 降级）。"""

    @pytest.mark.asyncio
    async def test_tools_record_matches_live_after_ref_degradation(
        self, sm, root, monkeypatch
    ):
        from wing.session import AgentOverride
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
        compact_record = _record_with_content(child_records, "[Compact] summary")
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
            for key in ("parent_uuid", "unzip_last_uuid", "usage_boundary_uuid"):
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


# ============================================================
# token 口径：usage 信任边界（issue #247）
# ============================================================

#: 「压缩前窗口」的服务端口径（换入之后不得再被当成当前 context 大小）。
PRE_COMPACT_TOKENS = 1100


class TestForkUsageBoundary:
    """fork 之后 token 口径不得把压缩前的 usage 当成当前窗口的大小。

    压缩换入的重链接保留区里带着**压缩前**窗口的 `prompt_tokens`（审计事实，有意
    保留）；口径锚在压缩节点的 `usage_boundary_uuid`（换入时刻的链尖）上。fork 点
    落在被保留的 tail 中间时边界在前缀之外——那时必须"一条都不采信"，而不是退回
    「没有边界 → 信任保留区 usage」这条历史口径（那正是 issue #247 的错误口径，
    而且子会话第一条请求就会命中）。
    """

    @staticmethod
    def _seed_stale_tail(session) -> None:
        """压缩换入后、保留区里带压缩前 usage 的链（记录顺序即 append 序）。

        ``old1 → old2 ─┬─ [Compact](cu, unzip=old2, boundary=t2)
                        └─ tail1(assistant, usage=1100) → tail2(user)``
        """
        cm = session.context_manager
        old1 = Message(role="user", content="old1")
        old2 = Message(role="assistant", content="old2")
        cm.add_message(old1)
        cm.add_message(old2)

        compact = Message(
            role="assistant", content="[Compact] summary", parent_uuid=None
        )
        compact.uuid = "cu"
        compact.unzip_last_uuid = old2.uuid
        compact.usage_boundary_uuid = "t2"  # 换入时刻的链尖 = tail2
        tail1 = Message(
            role="assistant",
            content="tail1",
            parent_uuid="cu",
            usage=LLMUsage(prompt_tokens=PRE_COMPACT_TOKENS, completion_tokens=1),
        )
        tail1.uuid = "t1"
        tail2 = Message(role="user", content="tail2", parent_uuid="t1")
        tail2.uuid = "t2"
        for node in (compact, tail1, tail2):
            cm._messages.append_detached(node)
        cm._messages.set_tip("t2")
        # 源会话：边界就是链尖 → 一条都不采信（压缩后还没落过新 usage）。
        assert cm._last_prompt_tokens() is None

    @pytest.mark.asyncio
    async def test_fork_inside_tail_does_not_revive_stale_usage(self, sm, root):
        """fork 点在保留区中间 → 子会话不信压缩前的 usage（边界钉到子链尖）。"""
        session = sm.create_session()
        self._seed_stale_tail(session)

        result = sm.fork_session(session.session_id, "t2")
        assert result is not None
        child, _ = result
        child_records = _records(root, child.session_id)
        child_uuids = {record["uuid"] for record in child_records}

        compact_record = _record_with_content(child_records, "[Compact] summary")
        # 边界被钉到前缀末条记录（= 子会话的活跃链尖），不是删键、也不是跨会话 uuid。
        assert compact_record["usage_boundary_uuid"] == child_records[-1]["uuid"]
        assert compact_record["usage_boundary_uuid"] in child_uuids

        # 子会话链：压缩节点 + 带压缩前 usage 的 tail1（审计事实照旧随行）。
        window = child.context_manager.get_context_window()
        assert _chain_contents(window) == ["[Compact] summary", "tail1"]
        tail = window[-1]
        assert tail.usage is not None
        assert tail.usage.prompt_tokens == PRE_COMPACT_TOKENS

        # 口径：一条都不采信（等价于"链上的东西全部早于那次换入"）。
        assert child.context_manager._last_prompt_tokens() is None
        # 之后落链的真实 usage 立即生效（不是永久退回估算）。
        child.context_manager.add_message(
            Message(
                role="assistant",
                content="post fork",
                usage=LLMUsage(prompt_tokens=42, completion_tokens=1),
            )
        )
        assert child.context_manager._last_prompt_tokens() == 42


# ============================================================
# 复制路径的落盘规范形
# ============================================================


class TestForkCopiesCanonicalShape:
    def test_legacy_source_records_are_canonicalised_on_copy(self):
        """源记录是旧形态（扁平字段 + null）时，复制出的子记录走规范形。

        fork 直接复用源记录的原始 dict（不经 ``_to_record``）——不过规范形这
        一道，存量冗余会被逐字节搬进子会话，越 fork 越多。
        """
        from wing.session.manager import _remap_record_uuids

        legacy = [
            {
                "role": "assistant",
                "content": "hi",
                "reasoning_content": "hmm",
                "tool_calls": [{"id": "c1", "name": "Bash", "arguments": {}}],
                "content_blocks": [{"type": "text", "text": "hi"}],
                "parent_uuid": None,
                "uuid": "u1",
                "ts": "2026-01-01T00:00:00",
            },
            {
                "role": "assistant",
                "content": "flat-only",
                "uuid": "u2",
                "ts": "2026-01-01T00:00:00",
            },
        ]
        first, flat_only = _remap_record_uuids(legacy)

        for key in ("content", "reasoning_content", "tool_calls"):
            assert key not in first, key
        assert "parent_uuid" not in first  # null 不写
        assert first["ts"] == "2026-01-01T00:00:00"  # 其余字段原样
        # 无 content_blocks 的记录里扁平字段是唯一存储——不剥
        assert flat_only["content"] == "flat-only"
        # 源 dict 未被就地改写（记录对读取方只读）
        assert legacy[0]["content"] == "hi"


class TestForkDraftFromDerivedContent:
    """fork 的 draft 必须从 records 派生（assistant 的 content 不落盘）。"""

    @pytest.mark.asyncio
    async def test_fork_at_compact_node_returns_its_text(self, sm, root):
        """目标 = 压缩节点（assistant + content_blocks）→ draft 是摘要文本。"""
        session = sm.create_session()
        _seed_compact(session)

        result = sm.fork_session(session.session_id, "cu")
        assert result is not None
        child, draft = result
        assert draft == "[Compact] summary"
        # 记录：压缩前区间随行走，压缩节点本身不进前缀
        child_records = _records(root, child.session_id)
        assert _contents(child_records) == ["old1", "old2"]

    @pytest.mark.asyncio
    async def test_fork_at_assistant_message_returns_its_text(self, sm, root):
        """目标 = 普通 assistant 记录 → draft 仍是它派生出的文本。"""
        session = sm.create_session()
        ids = _seed_compact(session)

        result = sm.fork_session(session.session_id, ids["old2"])
        assert result is not None
        _, draft = result
        assert draft == "old2"

    def test_fork_slice_derives_draft_from_blocks(self):
        """`_fork_slice` 直接吃 raw record：canonical 记录也要给出 draft。"""
        from wing.session.manager import _fork_slice

        records = [
            {"role": "user", "content": "hi", "uuid": "u1"},
            {
                "role": "assistant",
                "uuid": "a1",
                "content_blocks": [
                    {"type": "thinking", "thinking": "hmm"},
                    {"type": "text", "text": "reply"},
                ],
            },
        ]
        prefix, draft = _fork_slice(iter(records), "a1")
        assert draft == "reply"
        assert [r["uuid"] for r in prefix] == ["u1"]

    def test_fork_slice_draft_falls_back_for_corrupt_record(self):
        """脏记录按"无文本"降级——读请求不因一条坏记录整单失败。

        两种脏法都要兜住：块数组形态不合法（校验抛），以及 ``content`` 不是
        字符串（``_route_flat`` 把扁平值塞进私有属性、不做类型校验）。
        """
        from wing.session.manager import _fork_slice

        broken_blocks = [
            {"role": "assistant", "uuid": "a1", "content_blocks": "not-a-list"}
        ]
        _, draft = _fork_slice(iter(broken_blocks), "a1")
        assert draft == ""

        non_string = [{"role": "user", "uuid": "u1", "content": 123}]
        _, draft = _fork_slice(iter(non_string), "u1")
        assert draft == ""


class TestRemapOutOfPrefixReferences:
    def test_out_of_prefix_parent_is_dropped_not_nulled(self):
        """前缀外引用 → 删键（缺席即默认），不把 null 写进子会话记录。"""
        from wing.session.manager import _remap_record_uuids

        records = [
            {
                "role": "user",
                "uuid": "u1",
                "content": "hi",
                "parent_uuid": "outside-prefix",
                "unzip_last_uuid": "outside-prefix",
            }
        ]
        (clone,) = _remap_record_uuids(records)
        assert "parent_uuid" not in clone
        assert "unzip_last_uuid" not in clone
        assert clone["uuid"] != "u1"
        # 源记录未被就地改写
        assert records[0]["parent_uuid"] == "outside-prefix"

    def test_out_of_prefix_usage_boundary_is_pinned_to_prefix_tip(self):
        """usage 边界指向前缀外 → 钉到前缀末条记录（不是删键）。

        删键会让读取侧按「没有边界」降级成**信任**保留区里的压缩前 usage，正是
        issue #247 的错口径；钉到前缀末条记录等于「子会话链上的东西全部早于那次
        换入」——一条都不采信。
        """
        from wing.session.manager import _remap_record_uuids

        records = [
            {
                "role": "assistant",
                "content": "[Compact] summary",
                "uuid": "c1",
                "unzip_last_uuid": "m1",
                "usage_boundary_uuid": "outside-prefix",
            },
            {"role": "user", "uuid": "m1", "content": "beta", "parent_uuid": "c1"},
        ]
        compact_clone, beta_clone = _remap_record_uuids(records)
        assert compact_clone["usage_boundary_uuid"] == beta_clone["uuid"]
        assert compact_clone["unzip_last_uuid"] == beta_clone["uuid"]

    def test_inside_prefix_usage_boundary_is_remapped(self):
        """边界在前缀内 → 与 parent / unzip 同样走重映射（不留源会话 uuid）。"""
        from wing.session.manager import _remap_record_uuids

        records = [
            {
                "role": "assistant",
                "content": "[Compact] summary",
                "uuid": "c1",
                "unzip_last_uuid": "m1",
                "usage_boundary_uuid": "m2",
            },
            {"role": "user", "uuid": "m1", "content": "beta", "parent_uuid": "c1"},
            {
                "role": "assistant",
                "uuid": "m2",
                "content": "beta reply",
                "parent_uuid": "m1",
            },
        ]
        compact_clone, beta_clone, tip_clone = _remap_record_uuids(records)
        assert compact_clone["usage_boundary_uuid"] == tip_clone["uuid"]
        assert compact_clone["unzip_last_uuid"] == beta_clone["uuid"]
