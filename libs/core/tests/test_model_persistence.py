"""模型绑定（provider + model）持久化与 resume 还原的回归测试。

覆盖：
  - 显式动作落盘：模型切换 / 创建 override / 模板切换 / fork 快照
  - 跨进程重启（同一存储根目录新建 SessionManager）后的 resume 还原
  - 降级容错：provider 不可解析、半写记录
  - 旧数据兼容与「未显式动作不写记录」
  - 恢复后同步/初始化事件携带还原后的模型
"""

from __future__ import annotations

import json
import logging
from collections.abc import Iterator
from pathlib import Path

import pytest

from wing.agent_template import AgentTemplate
from wing.event import SessionInitEvent, SyncSessionEvent
from wing.event_bus import event_bus
from wing.gateway.protocol import AgentOverride
from wing.schema import Message
from wing.session_manager import SessionManager
from wing.store import FileSessionStore, SessionMetadata


@pytest.fixture
def root(tmp_path: Path) -> Path:
    """文件后端存储根目录（Gateway 的 session 目录）。"""
    return tmp_path / "sessions"


@pytest.fixture
def sm(root: Path) -> SessionManager:
    return SessionManager({"file": FileSessionStore(root)})


@pytest.fixture(autouse=True)
def _clean_event_bus():
    """隔离 EventBus（全局单例），避免订阅跨测试串味。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def received() -> Iterator[list]:
    """收集所有事件的全局订阅者。"""
    events: list = []
    event_bus.subscribe(events.append)
    yield events
    event_bus.unsubscribe(events.append)


@pytest.fixture
def wing_logs() -> Iterator[list[logging.LogRecord]]:
    """捕获 wing logger 的 WARNING 记录（log.propagate=False，需自挂 handler）。"""
    records: list[logging.LogRecord] = []

    class _Sink(logging.Handler):
        def emit(self, record: logging.LogRecord) -> None:
            records.append(record)

    handler = _Sink(level=logging.WARNING)
    logger = logging.getLogger("wing")
    logger.addHandler(handler)
    yield records
    logger.removeHandler(handler)


def _restart(root: Path) -> SessionManager:
    """在同一存储根目录上新建 SessionManager——模拟 Gateway 重启。"""
    return SessionManager({"file": FileSessionStore(root)})


def _seed(session, *contents: str) -> None:
    """向 session 的上下文写入消息（不经 LLM）。"""
    for i, content in enumerate(contents):
        role = "user" if i % 2 == 0 else "assistant"
        session.context_manager.add_message(Message(role=role, content=content))


def _metadata(root: Path, sid: str) -> SessionMetadata:
    loaded = FileSessionStore(root).load_metadata(sid)
    assert loaded is not None, f"metadata not found for {sid}"
    return loaded


class TestExplicitSwitchPersists:
    """模型切换（session/update）落盘并在重启后还原。"""

    @pytest.mark.asyncio
    async def test_switch_model_and_provider_restored_after_restart(self, sm, root):
        session = sm.create_session()
        sid = session.session_id

        await session.update_state(model="qwen3-max", provider_name="alt")

        meta = _metadata(root, sid)
        assert (meta.model_name, meta.provider_name) == ("qwen3-max", "alt")

        # 重启：同一 store 根目录上的新 SessionManager
        restored = _restart(root).resume_session(sid)
        assert restored.agent.model == "qwen3-max"
        assert restored.agent.model_provider.name == "alt"

    @pytest.mark.asyncio
    async def test_switch_model_only_keeps_current_provider(self, sm, root):
        session = sm.create_session()
        sid = session.session_id
        provider_name = session.agent.model_provider.name

        await session.update_state(model="qwen3-max")

        meta = _metadata(root, sid)
        assert meta.model_name == "qwen3-max"
        assert meta.provider_name == provider_name

        restored = _restart(root).resume_session(sid)
        assert restored.agent.model == "qwen3-max"
        assert restored.agent.model_provider.name == provider_name


class TestCreateOverridePersists:
    """创建时的 AgentOverride 是显式动作，同样落盘。"""

    @pytest.mark.asyncio
    async def test_create_with_override_records_and_restores(self, sm, root):
        session = sm.create_session(
            agent_override=AgentOverride(model="gpt-4o-mini", provider="alt")
        )
        sid = session.session_id

        meta = _metadata(root, sid)
        assert (meta.model_name, meta.provider_name) == ("gpt-4o-mini", "alt")

        restored = _restart(root).resume_session(sid)
        assert restored.agent.model == "gpt-4o-mini"
        assert restored.agent.model_provider.name == "alt"


class TestTemplateSwitchPersists:
    """模板切换覆写模型记录（新模板的生效模型）。"""

    @pytest.mark.asyncio
    async def test_template_switch_overwrites_record(self, sm, root):
        session = sm.create_session()
        sid = session.session_id
        # 先留下一个旧的显式记录，验证会被模板切换覆写
        await session.update_state(model="qwen3-max", provider_name="alt")

        template = AgentTemplate(name="coder", model="claude-x", provider_name="alt")
        await session.switch_template(template)

        meta = _metadata(root, sid)
        assert meta.template_name == "coder"
        assert (meta.model_name, meta.provider_name) == ("claude-x", "alt")

        # coder 不在 config 中 → resume 回落默认模板，但模型记录优先还原
        restored = _restart(root).resume_session(sid)
        assert restored.agent.model == "claude-x"
        assert restored.agent.model_provider.name == "alt"


class TestForkSnapshot:
    """fork 记录源 session 在 fork 时刻的生效模型（快照）。"""

    @pytest.mark.asyncio
    async def test_fork_after_switch_records_source_model(self, sm, root):
        source = sm.create_session()
        await source.update_state(model="qwen3-max", provider_name="alt")
        _seed(source, "hello", "question")
        target = source.context_manager.get_context_window()[-1].uuid

        child, _ = sm.fork_session(source.session_id, target)
        assert child is not None

        meta = _metadata(root, child.session_id)
        assert (meta.model_name, meta.provider_name) == ("qwen3-max", "alt")

        restored = _restart(root).resume_session(child.session_id)
        assert restored.agent.model == "qwen3-max"
        assert restored.agent.model_provider.name == "alt"

    @pytest.mark.asyncio
    async def test_fork_without_switch_snapshots_effective_model(self, sm, root):
        source = sm.create_session()
        _seed(source, "hello", "question")
        target = source.context_manager.get_context_window()[-1].uuid

        child, _ = sm.fork_session(source.session_id, target)
        assert child is not None

        # 即使源从未显式切换，子 session 也记录 fork 时刻的生效模型
        meta = _metadata(root, child.session_id)
        assert meta.model_name == source.agent.model
        assert meta.provider_name == source.agent.model_provider.name


class TestDegradation:
    """记录不可用时的降级：不阻断 resume，且不丢记录。"""

    @pytest.mark.asyncio
    async def test_unknown_provider_falls_back_and_keeps_record(
        self, sm, root, wing_logs
    ):
        session = sm.create_session()
        sid = session.session_id
        session.store.save_metadata(
            sid,
            SessionMetadata(model_name="ghost-model", provider_name="ghost"),
        )

        restored = _restart(root).resume_session(sid)

        assert restored.agent.model == sm.template_manager.default.model
        assert any("cannot restore model" in r.getMessage() for r in wing_logs)
        # 记录保留：config 修复后仍能还原
        meta = _metadata(root, sid)
        assert (meta.model_name, meta.provider_name) == ("ghost-model", "ghost")

    @pytest.mark.asyncio
    async def test_partial_record_treated_as_no_record(self, sm, root):
        session = sm.create_session()
        sid = session.session_id
        session.store.save_metadata(sid, SessionMetadata(model_name="half-written"))

        restored = _restart(root).resume_session(sid)
        assert restored.agent.model == sm.template_manager.default.model


class TestCompatibility:
    """旧数据与「未显式动作不写记录」。"""

    @pytest.mark.asyncio
    async def test_legacy_metadata_resumes_with_template_model(self, sm, root):
        template_model = sm.template_manager.default.model
        session = sm.create_session()
        sid = session.session_id
        _seed(session, "hello")

        # 模拟本次变更前写下的 metadata（无模型字段）
        (root / sid / "metadata.json").write_text(
            json.dumps({"session_name": "old"}), encoding="utf-8"
        )

        restored = _restart(root).resume_session(sid)
        assert restored.agent.model == template_model

    @pytest.mark.asyncio
    async def test_untouched_session_writes_no_record(self, sm, root):
        session = sm.create_session()
        sid = session.session_id
        session.touch_last_interaction()  # 触发一次 metadata 落盘

        raw = json.loads((root / sid / "metadata.json").read_text(encoding="utf-8"))
        assert "model_name" not in raw
        assert "provider_name" not in raw


class TestRestoreEvents:
    """恢复后的事件携带还原模型（前端首帧与重启前一致）。"""

    @pytest.mark.asyncio
    async def test_resume_pushes_restored_model_in_events(self, received):
        from wing.runtime import WingRuntime

        rt1 = WingRuntime()
        session = rt1.create_session()
        sid = session.session_id
        await session.update_state(model="qwen3-max", provider_name="alt")

        # 模拟 Gateway 重启：新 runtime 从磁盘恢复同一 session
        rt2 = WingRuntime()
        restored = rt2.resume_session(sid)
        assert restored.agent.model == "qwen3-max"

        received.clear()
        rt2.subscribe("client-1", sid)

        syncs = [e for e in received if isinstance(e, SyncSessionEvent)]
        inits = [e for e in received if isinstance(e, SessionInitEvent)]
        assert len(syncs) == 1 and len(inits) == 1

        agent = syncs[0].agent
        assert agent is not None
        assert agent.model_name == "qwen3-max"
        assert agent.provider_name == "alt"
        assert inits[0].model == "qwen3-max"
