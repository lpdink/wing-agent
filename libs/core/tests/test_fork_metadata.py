"""fork 元数据持久化回归测试。

覆盖 fork bug 的结构性修复：fork 时 workspace/forked_from/template_name/
last_interaction 一次写全；重启后 resume 恢复 workspace 与模板；
forked session 首条消息获得标题且不覆盖 forked_from。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.schema import Message
from wing.session_manager import SessionManager
from wing.store import FileSessionStore, MemorySessionStore


@pytest.fixture
def file_sm(tmp_path: Path):
    """文件后端 SessionManager。"""
    return SessionManager({"file": FileSessionStore(tmp_path / "sessions")})


def _seed(session, *contents: str) -> None:
    """向 session 的上下文写入消息（不经 LLM）。"""
    for i, content in enumerate(contents):
        role = "user" if i % 2 == 0 else "assistant"
        session.context_manager.add_message(Message(role=role, content=content))


class TestForkMetadata:
    @pytest.mark.asyncio
    async def test_fork_persists_complete_metadata(
        self, file_sm: SessionManager, tmp_path: Path
    ):
        """fork 一次性持久化 workspace/forked_from/template_name/last_interaction。"""
        ws = tmp_path / "ws"
        ws.mkdir()
        source = file_sm.create_session(workspace=str(ws))
        _seed(source, "hello", "hi", "question")

        last_uuid = source.context_manager.get_context_window()[-1].uuid
        new_session, draft = file_sm.fork_session(source.session_id, last_uuid)  # ty: ignore[invalid-argument-type, not-iterable]
        assert draft == "question"

        meta = source.store.load_metadata(new_session.session_id)
        assert meta is not None
        assert meta.workspace == str(ws)
        assert meta.forked_from == source.session_id
        assert meta.template_name == source.template_name == "default"
        assert meta.last_interaction is not None

    @pytest.mark.asyncio
    async def test_forked_session_resume_after_restart(
        self, file_sm: SessionManager, tmp_path: Path
    ):
        """模拟重启（新 SM 实例）后 resume forked session，workspace 与模板恢复。"""
        ws = tmp_path / "ws"
        ws.mkdir()
        source = file_sm.create_session(workspace=str(ws))
        _seed(source, "hello", "hi")

        last_uuid = source.context_manager.get_context_window()[-1].uuid
        new_session, _ = file_sm.fork_session(source.session_id, last_uuid)  # ty: ignore[invalid-argument-type, not-iterable]
        new_sid = new_session.session_id

        # 模拟进程重启：同一 store 根目录，全新 SM 实例
        restarted = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        resumed = restarted.resume_session(new_sid)

        assert resumed.session_workspace == str(ws)
        assert resumed.template_name == "default"
        # ContextManager 的 workspace 同步恢复（相对路径 skills/rules 解析依赖它）
        assert resumed.context_manager._workspace == ws.resolve()
        # 消息链完整恢复（fork 复制 target 之前的子链，target 成为 draft）
        assert len(resumed.context_manager.get_context_window()) == 1

    @pytest.mark.asyncio
    async def test_forked_session_first_message_title_preserves_forked_from(
        self, file_sm: SessionManager, tmp_path: Path
    ):
        """forked session 首条消息获得标题，forked_from/workspace 不被覆盖。"""
        ws = tmp_path / "ws"
        ws.mkdir()
        source = file_sm.create_session(workspace=str(ws))
        _seed(source, "hello", "hi")

        last_uuid = source.context_manager.get_context_window()[-1].uuid
        new_session, _ = file_sm.fork_session(source.session_id, last_uuid)  # ty: ignore[invalid-argument-type, not-iterable]

        # 首条消息标题逻辑（post 的前置步骤，不经 LLM）：
        # _check 变更模型，touch 一次性落盘
        new_session._check_first_message_metadata("my new question")
        new_session.touch_last_interaction()

        meta = source.store.load_metadata(new_session.session_id)
        assert meta is not None
        assert meta.session_name == "my new question"
        assert meta.forked_from == source.session_id
        assert meta.workspace == str(ws)

    @pytest.mark.asyncio
    async def test_fork_does_not_corrupt_source(
        self, file_sm: SessionManager, tmp_path: Path
    ):
        """fork 不污染源 session：uuid 不变，链状态完好，可再次 fork。"""
        source = file_sm.create_session()
        _seed(source, "hello", "hi", "world")

        uuids_before = [m.uuid for m in source.context_manager.get_context_window()]
        target = uuids_before[-1]

        file_sm.fork_session(source.session_id, target)  # ty: ignore[invalid-argument-type]

        uuids_after = [m.uuid for m in source.context_manager.get_context_window()]
        assert uuids_after == uuids_before
        assert source.context_manager._messages.find(target) is not None  # ty: ignore[invalid-argument-type]

        # 二次 fork 仍可用原 uuid 定位
        result = file_sm.fork_session(source.session_id, target)  # ty: ignore[invalid-argument-type]
        assert result is not None

    @pytest.mark.asyncio
    async def test_fork_inherits_memory_backend(self):
        """memory session fork 出 memory session。"""
        sm = SessionManager({"memory": MemorySessionStore()}, default_backend="memory")
        source = sm.create_session()
        _seed(source, "hello", "hi")

        last_uuid = source.context_manager.get_context_window()[-1].uuid
        new_session, _ = sm.fork_session(source.session_id, last_uuid)  # ty: ignore[invalid-argument-type, not-iterable]
        assert new_session.store.name == "memory"
        assert len(new_session.context_manager.get_context_window()) == 1


class TestResumeTemplate:
    @pytest.mark.asyncio
    async def test_resume_restores_persisted_template(self, tmp_path: Path):
        """resume 默认使用 metadata 中持久化的模板。"""
        from wing.agent_template import AgentTemplate

        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})
        # 注入第二个模板
        coder = AgentTemplate(name="coder", model="gpt-4")
        sm._template_manager._templates["coder"] = coder

        session = sm.create_session(template_name="coder")
        _seed(session, "hello")
        session.touch_last_interaction()  # 触发 metadata 持久化（正常流程由 post 触发）
        sid = session.session_id

        # 模拟重启
        restarted = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        restarted._template_manager._templates["coder"] = coder
        resumed = restarted.resume_session(sid)
        assert resumed.template_name == "coder"

    @pytest.mark.asyncio
    async def test_resume_explicit_template_wins(self, tmp_path: Path):
        """显式传入模板优先于 metadata。"""
        from wing.agent_template import AgentTemplate

        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})
        coder = AgentTemplate(name="coder", model="gpt-4")
        sm._template_manager._templates["coder"] = coder

        session = sm.create_session(template_name="coder")
        _seed(session, "hello")

        del sm._sessions[session.session_id]
        resumed = sm.resume_session(
            session.session_id, template=sm.template_manager.default
        )
        assert resumed.template_name == "default"

    @pytest.mark.asyncio
    async def test_resume_legacy_metadata_falls_back_default(self, tmp_path: Path):
        """老 metadata（无 template_name）回退默认模板。"""
        store = FileSessionStore(tmp_path / "sessions")
        sm = SessionManager({"file": store})
        session = sm.create_session()
        _seed(session, "hello")
        session.touch_last_interaction()
        # 抹掉 template_name 模拟老数据
        meta = store.load_metadata(session.session_id)
        assert meta is not None
        meta.template_name = None
        meta.session_name = "legacy"
        store.save_metadata(session.session_id, meta)

        del sm._sessions[session.session_id]
        resumed = sm.resume_session(session.session_id)
        assert resumed.template_name == "default"


class TestLegacyCompat:
    """重构前磁盘布局零迁移兼容。"""

    @pytest.mark.asyncio
    async def test_resume_legacy_session_layout(self, tmp_path: Path):
        """手工构造重构前的老 session（history.jsonl + 老格式 metadata.json），
        resume 与 list 行为正确。"""
        import json
        from uuid import uuid4

        root = tmp_path / "sessions"
        sid = "20250101-000000-legacy01"
        session_dir = root / sid
        session_dir.mkdir(parents=True)

        # 老格式 metadata：无 template_name / forked_from 字段
        (session_dir / "metadata.json").write_text(
            json.dumps({"session_name": "old session", "workspace": "/tmp/oldws"}),
            encoding="utf-8",
        )

        # history.jsonl：两条链式消息（ts 为老格式附加字段）
        u1, u2 = str(uuid4()), str(uuid4())
        records = [
            {
                "role": "user",
                "content": "hello",
                "uuid": u1,
                "parent_uuid": None,
                "unzip_last_uuid": None,
                "ts": "2025-01-01T00:00:00",
            },
            {
                "role": "assistant",
                "content": "hi",
                "uuid": u2,
                "parent_uuid": u1,
                "unzip_last_uuid": None,
                "ts": "2025-01-01T00:00:01",
            },
        ]
        (session_dir / "history.jsonl").write_text(
            "\n".join(json.dumps(r) for r in records) + "\n", encoding="utf-8"
        )
        (session_dir / "newest.json").write_text(
            json.dumps([r for r in records]), encoding="utf-8"
        )

        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.resume_session(sid)

        assert session.session_workspace == "/tmp/oldws"
        assert session.session_name == "old session"
        assert session.template_name == "default"  # 无 template_name → 默认
        assert len(session.context_manager.get_context_window()) == 2

        infos = sm.list_sessions()
        assert any(i.id == sid and i.name == "old session" for i in infos)
