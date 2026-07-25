"""非持久化（memory 后端）session 回归测试。

覆盖：backend 选择、memory session 全功能（消息/fork/rewind）不落盘、
跨后端列表聚合、未知 backend 拒绝。
"""

from __future__ import annotations

import pytest

from wing.schema import Message
from wing.session_manager import SessionManager
from wing.store import MemorySessionStore


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


def _sessions_root():
    from wing.config import get_config

    return get_config().sessions.resolved_path()


def _dir_set():
    root = _sessions_root()
    return set(root.iterdir()) if root.exists() else set()


class TestBackendSelection:
    @pytest.mark.asyncio
    async def test_default_is_file(self, runtime):
        session = runtime.create_session()
        assert session.store.name == "file"

    @pytest.mark.asyncio
    async def test_memory_backend(self, runtime):
        session = runtime.create_session(backend="memory")
        assert session.store.name == "memory"

    @pytest.mark.asyncio
    async def test_unknown_backend_rejected(self, runtime):
        with pytest.raises(ValueError, match="redis"):
            runtime.create_session(backend="redis")

    def test_sm_unknown_backend_message_lists_available(self):
        sm = SessionManager({"memory": MemorySessionStore()}, default_backend="memory")
        with pytest.raises(ValueError, match="memory"):
            sm.create_session(backend="pg")


class TestMemorySessionNoDisk:
    @pytest.mark.asyncio
    async def test_full_flow_no_files(self, runtime):
        """memory session：消息、rewind、fork 全程不产生文件。"""
        before = _dir_set()

        session = runtime.create_session(backend="memory")
        cm = session.context_manager
        cm.add_message(Message(role="user", content="hello"))
        cm.add_message(Message(role="assistant", content="hi"))
        cm.add_message(Message(role="user", content="q"))

        # metadata 写入路径（正常流程由 post 触发）
        session._check_first_message_metadata("hello")
        session.touch_last_interaction()

        # fork（复制 target 之前的子链）
        last_uuid = cm.get_context_window()[-1].uuid
        new_session, _ = runtime.fork_session(session.session_id, last_uuid)
        assert new_session.store.name == "memory"
        assert len(new_session.context_manager.get_context_window()) == 2

        # rewind
        first_uuid = cm.get_context_window()[0].uuid
        runtime.rewind_session(session.session_id, first_uuid)

        # resume（同进程内）
        resumed = runtime.resume_session(new_session.session_id)
        assert resumed is new_session

        assert _dir_set() == before

    @pytest.mark.asyncio
    async def test_memory_session_not_resumable_cross_process(self, runtime, tmp_path):
        """memory session 跨进程（新 runtime）不可 resume。"""
        session = runtime.create_session(backend="memory")
        session.context_manager.add_message(Message(role="user", content="hi"))
        sid = session.session_id

        from wing.runtime import WingRuntime

        restarted = WingRuntime()
        with pytest.raises(LookupError):
            restarted.resume_session(sid)


class TestListBothBackends:
    @pytest.mark.asyncio
    async def test_list_aggregates_stores(self, runtime):
        """list_sessions 跨后端聚合。"""
        file_session = runtime.create_session()
        file_session.context_manager.add_message(
            Message(role="user", content="file msg")
        )

        mem_session = runtime.create_session(backend="memory")
        mem_session.context_manager.add_message(Message(role="user", content="mem msg"))

        infos = runtime.list_sessions()
        by_id = {i.id: i for i in infos}
        assert file_session.session_id in by_id
        assert mem_session.session_id in by_id
        # 标题回退：首条用户消息
        assert by_id[mem_session.session_id].name == "mem msg"
