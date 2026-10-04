"""Session 状态模型单元测试。

覆盖：
- agent.status 推导规则（idle/working/waiting 及优先级）
- Session.status 委托 agent
- SessionManager.list_sessions 的 status 填充（inactive vs live）与排序口径
  （活跃优先 + 组内最后交互时间降序；workspace 不参与；时间缺失回退 id 前缀）
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
    last_interaction: str | None,
    workspace: str | None = None,
    name: str = "test session",
) -> None:
    """在磁盘上构造一个合法 session 目录（history.jsonl + metadata.json）。

    ``last_interaction=None`` 表示 metadata 里**没有**该键（不是 null）——
    排序回退到 session id 前缀的那条路径。
    """
    session_dir = sessions_path / session_id
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / "history.jsonl").write_text(
        json.dumps({"role": "user", "content": name, "uuid": f"{session_id}-u1"}) + "\n"
    )
    metadata: dict = {"session_name": name}
    if last_interaction is not None:
        metadata["last_interaction"] = last_interaction
    if workspace is not None:
        metadata["workspace"] = workspace
    (session_dir / "metadata.json").write_text(json.dumps(metadata))


def _make_manager(sessions_path: Path):
    from wing.session_manager import SessionManager
    from wing.store import FileSessionStore

    return SessionManager({"file": FileSessionStore(sessions_path)})


def _mark_active(sm, session_id: str, status: str = "idle") -> None:
    """把 session 标记为「已加载进内存」（active）并给定 live 状态。"""
    from unittest.mock import MagicMock

    live = MagicMock()
    live.status = status
    sm._sessions[session_id] = live


def _ids(sm) -> list[str]:
    return [s.id for s in sm.list_sessions()]


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
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "loaded", "2025-01-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, "loaded", "working")

        result = sm.list_sessions()

        assert len(result) == 1
        assert result[0].status == "working"

    def test_mixed_loaded_and_disk(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "a", "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, "b", "2025-02-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, "a")

        result = {s.id: s.status for s in sm.list_sessions()}

        assert result == {"a": "idle", "b": "inactive"}


class TestListSessionsOrder:
    """排序口径：活跃（已在内存）优先 + 组内最后交互时间降序，workspace 不参与。"""

    def test_active_comes_first_even_when_inactive_is_newer(self, tmp_path: Path):
        # 唯一能区分「活跃优先」与「纯时间降序」的形状：active 的时间更旧。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "active-old", "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, "inactive-new", "2025-06-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, "active-old")

        result = sm.list_sessions()

        assert [s.id for s in result] == ["active-old", "inactive-new"]
        assert [s.status for s in result] == ["idle", "inactive"]

    def test_every_live_status_counts_as_active(self, tmp_path: Path):
        # active = 已加载进内存：working / waiting / idle 都是，只有 inactive 不在前。
        sessions_path = tmp_path / "sessions"
        for name, stamp in (
            ("idle-session", "2025-01-01T00:00:00"),
            ("working-session", "2025-01-01T00:00:01"),
            ("waiting-session", "2025-01-01T00:00:02"),
            ("disk-session", "2025-01-01T00:00:03"),
        ):
            _write_session_dir(sessions_path, name, stamp)

        sm = _make_manager(sessions_path)
        _mark_active(sm, "idle-session", "idle")
        _mark_active(sm, "working-session", "working")
        _mark_active(sm, "waiting-session", "waiting")

        # 三个 active 都在最新的 inactive（时间 00:00:03）之前——组内仍是时间降序。
        assert _ids(sm) == [
            "waiting-session",
            "working-session",
            "idle-session",
            "disk-session",
        ]

    def test_group_order_is_time_descending(self, tmp_path: Path):
        # active 组内、inactive 组内都按 last_interaction 降序。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "active-old", "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, "active-new", "2025-02-01T00:00:00")
        _write_session_dir(sessions_path, "inactive-old", "2025-03-01T00:00:00")
        _write_session_dir(sessions_path, "inactive-new", "2025-04-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, "active-old")
        _mark_active(sm, "active-new")

        assert _ids(sm) == [
            "active-new",
            "active-old",
            "inactive-new",
            "inactive-old",
        ]

    def test_workspace_does_not_affect_order(self, tmp_path: Path):
        # 同一组时间戳，只把 workspace 调换：顺序随之改变就说明 workspace 仍在排序。
        left, right = tmp_path / "left", tmp_path / "right"
        for path, (a_ws, b_ws) in ((left, ("/a", "/b")), (right, ("/b", "/a"))):
            _write_session_dir(
                path, "active-old", "2025-01-01T00:00:00", workspace=a_ws
            )
            _write_session_dir(
                path, "inactive-new", "2025-06-01T00:00:00", workspace=b_ws
            )

        first, second = _make_manager(left), _make_manager(right)
        _mark_active(first, "active-old")
        _mark_active(second, "active-old")

        assert _ids(first) == ["active-old", "inactive-new"]
        assert _ids(second) == ["active-old", "inactive-new"]

    def test_missing_last_interaction_falls_back_to_id_prefix(self, tmp_path: Path):
        # metadata 没有 last_interaction → 用 id 前缀 YYYYMMDD-HHMMSS。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "20250101-101010-session", None)
        _write_session_dir(sessions_path, "20250601-101010-session", None)
        _write_session_dir(sessions_path, "20250301-101010-session", None)

        sm = _make_manager(sessions_path)

        assert _ids(sm) == [
            "20250601-101010-session",
            "20250301-101010-session",
            "20250101-101010-session",
        ]

    def test_id_prefix_fallback_keeps_active_first(self, tmp_path: Path):
        # 回退路径同样受「活跃优先」支配：active 的 id 前缀时间更旧也仍在前面。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "20250101-101010-session", None)
        _write_session_dir(sessions_path, "20250601-101010-session", None)

        sm = _make_manager(sessions_path)
        _mark_active(sm, "20250101-101010-session")

        assert _ids(sm) == ["20250101-101010-session", "20250601-101010-session"]

    def test_unparseable_timestamp_sorts_last(self, tmp_path: Path):
        # 时间与 id 前缀都取不到 → 按 0，排在所有可解析的会话之后（不抛）。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "no-time-session", None)
        _write_session_dir(sessions_path, "broken-time", "not-a-timestamp")
        _write_session_dir(sessions_path, "20250101-101010-session", None)

        sm = _make_manager(sessions_path)
        result = sm.list_sessions()

        assert [s.id for s in result] == [
            "20250101-101010-session",
            "no-time-session",
            "broken-time",
        ]
