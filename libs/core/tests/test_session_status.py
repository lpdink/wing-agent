"""Session 状态模型单元测试。

覆盖：
- agent.status 推导规则（idle/working/waiting 及优先级）
- Session.status 委托 agent
- SessionManager.list_sessions 时间降序 + status 填充（inactive vs live）
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest

from wing.agent import WingAgent
from wing.agent.inbox import Inbox


def _make_agent(working: bool = False) -> WingAgent:
    """构造最小化 WingAgent（绕过 __init__，避免启动 worker task）。

    仅设置 status property 依赖的属性：_inbox 与 _working。
    """
    agent = object.__new__(WingAgent)
    agent._inbox = Inbox()
    agent._working = working
    return agent


def _register_waiter(agent: WingAgent) -> None:
    """在 agent 上注册一个 feedback waiter（需运行中的事件循环）。"""
    agent._inbox._feedback_waiters["tc_stub"] = (
        asyncio.get_running_loop().create_future()
    )


class TestAgentStatus:
    def test_idle_by_default(self):
        agent = _make_agent()
        assert agent.status == "idle"

    def test_working_when_turn_in_progress(self):
        agent = _make_agent(working=True)
        assert agent.status == "working"

    @pytest.mark.asyncio
    async def test_waiting_when_feedback_pending(self):
        agent = _make_agent()
        _register_waiter(agent)
        assert agent.status == "waiting"

    @pytest.mark.asyncio
    async def test_waiting_takes_priority_over_working(self):
        # turn 进行中且阻塞在 ask → waiting 优先
        agent = _make_agent(working=True)
        _register_waiter(agent)
        assert agent.status == "waiting"

    @pytest.mark.asyncio
    async def test_back_to_working_after_feedback(self):
        agent = _make_agent(working=True)
        _register_waiter(agent)
        assert agent.status == "waiting"
        agent._inbox._feedback_waiters.clear()
        assert agent.status == "working"

    def test_back_to_idle_after_turn(self):
        agent = _make_agent(working=True)
        agent._working = False
        assert agent.status == "idle"


class TestSessionStatusDelegation:
    def test_session_status_delegates_to_agent(self, tmp_path: Path):
        from unittest.mock import MagicMock

        from wing.session import Session
        from wing.store import MemorySessionStore

        agent = _make_agent(working=True)
        # 补齐 Session.__init__ → agent.get_status() 所需属性
        agent.model = "gpt-4"
        agent._tools = {}
        agent.context_manager = MagicMock()
        agent.context_manager.get_context_stats.return_value = (0, 0)
        agent.context_manager.compactor = None
        agent.model_provider = MagicMock()
        agent.model_provider.thinking = False
        agent.model_provider.reasoning_effort = None

        mock_cm = MagicMock()
        session = Session(
            session_id="test-status",
            messages=MagicMock(),
            context_manager=mock_cm,
            agent=agent,
            store=MemorySessionStore(),
        )

        assert session.status == "working"
        agent._working = False
        assert session.status == "idle"


def _write_session_dir(
    sessions_path: Path,
    session_id: str,
    last_interaction: str,
    workspace: str | None = None,
    name: str = "test session",
) -> None:
    """在磁盘上构造一个合法 session 目录（history.jsonl + metadata.json）。"""
    session_dir = sessions_path / session_id
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / "history.jsonl").write_text(
        json.dumps({"role": "user", "content": name, "uuid": f"{session_id}-u1"}) + "\n"
    )
    metadata: dict = {"session_name": name, "last_interaction": last_interaction}
    if workspace is not None:
        metadata["workspace"] = workspace
    (session_dir / "metadata.json").write_text(json.dumps(metadata))


def _make_manager(sessions_path: Path):
    from wing.session_manager import SessionManager
    from wing.store import FileSessionStore

    return SessionManager({"file": FileSessionStore(sessions_path)})


class TestListSessions:
    def test_time_descending_order(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "old", "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, "new", "2025-06-01T00:00:00")
        _write_session_dir(sessions_path, "mid", "2025-03-01T00:00:00")

        sm = _make_manager(sessions_path)
        result = sm.list_sessions()

        assert [s.id for s in result] == ["new", "mid", "old"]

    def test_no_workspace_parameter(self):
        import inspect

        from wing.session_manager import SessionManager

        sig = inspect.signature(SessionManager.list_sessions)
        assert "workspace" not in sig.parameters

    def test_disk_only_session_is_inactive(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "on-disk", "2025-01-01T00:00:00")

        sm = _make_manager(sessions_path)
        result = sm.list_sessions()

        assert len(result) == 1
        assert result[0].status == "inactive"

    def test_loaded_session_reflects_live_status(self, tmp_path: Path):
        from unittest.mock import MagicMock

        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "loaded", "2025-01-01T00:00:00")

        sm = _make_manager(sessions_path)
        live_session = MagicMock()
        live_session.status = "working"
        sm._sessions["loaded"] = live_session

        result = sm.list_sessions()

        assert len(result) == 1
        assert result[0].status == "working"

    def test_mixed_loaded_and_disk(self, tmp_path: Path):
        from unittest.mock import MagicMock

        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "a", "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, "b", "2025-02-01T00:00:00")

        sm = _make_manager(sessions_path)
        live_session = MagicMock()
        live_session.status = "idle"
        sm._sessions["a"] = live_session

        result = {s.id: s.status for s in sm.list_sessions()}

        assert result == {"a": "idle", "b": "inactive"}
