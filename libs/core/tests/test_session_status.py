"""Session 状态模型单元测试。

覆盖：
- agent.status 推导规则（idle/working/waiting 及优先级）
- Session.status 委托 agent
- SessionManager.list_sessions 的 status 填充（inactive vs live）与排序口径
  （活跃优先 + 组内最后交互时间降序；workspace 不参与；时间缺失回退 id 前缀）
- SessionManager.list_live_sessions 的轻量投影（只含在内存会话、与 list 活跃组
  同序、行形状只有 id / status）
"""

from __future__ import annotations

import asyncio
import hashlib
import json
from pathlib import Path

import pytest

from wing.agent import WingAgent
from wing.agent.inbox import Inbox


def _sid(label: str) -> str:
    """测试用 session id：稳定、唯一、契合后端生成的既定格式。

    存储层把 session id 当路径组件并校验格式——不得再用 'old' / 'new'
    这类任意字符串当 id；需要 id 前缀时间语义的测试请显式写字面 id。
    """
    digest = hashlib.md5(label.encode()).hexdigest()[:8]
    return f"20250101-000000-{digest}"


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
        agent._provider_name = "default"
        agent._thinking = False
        agent._reasoning_effort = None

        mock_cm = MagicMock()
        session = Session(
            session_id="20250101-000000-abcdef06",
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
    from wing.session import SessionManager
    from wing.store import FileSessionStore

    return SessionManager({"file": FileSessionStore(sessions_path)})


def _mark_active(
    sm, session_id: str, status: str = "idle", *, last: str | None = None
) -> None:
    """把 session 标记为「已加载进内存」（active）并给定 live 状态。

    列表条目还投影模型四件套（`Session.model_binding`）——假会话照实给一份
    （未声明任何模型的降级形态），本文件断言的是状态与排序。

    ``last`` 是该会话的 ``last_interaction``（None = 缺失，排序回退 id 前缀）。
    """
    from unittest.mock import MagicMock

    from wing.session import ModelBinding

    live = MagicMock()
    live.status = status
    live.last_interaction = last
    live.model_binding.return_value = ModelBinding(
        model_id=None,
        model_name="stub-model",
        provider_name="p",
        model_display_name=None,
    )
    sm._sessions[session_id] = live


def _ids(sm) -> list[str]:
    return [s.id for s in sm.list_sessions()]


class TestListSessions:
    def test_time_descending_order(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("old"), "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, _sid("new"), "2025-06-01T00:00:00")
        _write_session_dir(sessions_path, _sid("mid"), "2025-03-01T00:00:00")

        sm = _make_manager(sessions_path)
        result = sm.list_sessions()

        assert [s.id for s in result] == [_sid("new"), _sid("mid"), _sid("old")]

    def test_no_workspace_parameter(self):
        import inspect

        from wing.session import SessionManager

        sig = inspect.signature(SessionManager.list_sessions)
        assert "workspace" not in sig.parameters

    def test_disk_only_session_is_inactive(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("on-disk"), "2025-01-01T00:00:00")

        sm = _make_manager(sessions_path)
        result = sm.list_sessions()

        assert len(result) == 1
        assert result[0].status == "inactive"

    def test_loaded_session_reflects_live_status(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("loaded"), "2025-01-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("loaded"), "working")

        result = sm.list_sessions()

        assert len(result) == 1
        assert result[0].status == "working"

    def test_mixed_loaded_and_disk(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("a"), "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, _sid("b"), "2025-02-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("a"))

        result = {s.id: s.status for s in sm.list_sessions()}

        assert result == {_sid("a"): "idle", _sid("b"): "inactive"}


class TestListSessionsOrder:
    """排序口径：活跃（已在内存）优先 + 组内最后交互时间降序，workspace 不参与。

    组内不引入状态优先级（waiting 不提前），并列时按 id 定序（全序，见
    `SessionManager.list_sessions` 的 docstring）。
    """

    def test_active_comes_first_even_when_inactive_is_newer(self, tmp_path: Path):
        # 唯一能区分「活跃优先」与「纯时间降序」的形状：active 的时间更旧。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("active-old"), "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, _sid("inactive-new"), "2025-06-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("active-old"))

        result = sm.list_sessions()

        assert [s.id for s in result] == [_sid("active-old"), _sid("inactive-new")]
        assert [s.status for s in result] == ["idle", "inactive"]

    def test_every_live_status_counts_as_active(self, tmp_path: Path):
        # active = 已加载进内存：working / waiting / idle 都是，只有 inactive 不在前。
        sessions_path = tmp_path / "sessions"
        for name, stamp in (
            (_sid("idle-session"), "2025-01-01T00:00:00"),
            (_sid("working-session"), "2025-01-01T00:00:01"),
            (_sid("waiting-session"), "2025-01-01T00:00:02"),
            (_sid("disk-session"), "2025-01-01T00:00:03"),
        ):
            _write_session_dir(sessions_path, name, stamp)

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("idle-session"), "idle")
        _mark_active(sm, _sid("working-session"), "working")
        _mark_active(sm, _sid("waiting-session"), "waiting")

        # 三个 active 都在最新的 inactive（时间 00:00:03）之前——组内仍是时间降序。
        assert _ids(sm) == [
            _sid("waiting-session"),
            _sid("working-session"),
            _sid("idle-session"),
            _sid("disk-session"),
        ]

    def test_waiting_gets_no_priority_within_the_active_group(self, tmp_path: Path):
        # 组内只看时间：更新的 idle 排在更旧的 waiting 之前。旧前端的 rank 键
        # （waiting > working > idle）会给出相反顺序——「正在等你回答」不再靠
        # 排序提前，靠状态图标（`?`）表达。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("waiting-old"), "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, _sid("idle-new"), "2025-06-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("waiting-old"), "waiting")
        _mark_active(sm, _sid("idle-new"), "idle")

        assert _ids(sm) == [_sid("idle-new"), _sid("waiting-old")]

    def test_group_order_is_time_descending(self, tmp_path: Path):
        # active 组内、inactive 组内都按 last_interaction 降序。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("active-old"), "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, _sid("active-new"), "2025-02-01T00:00:00")
        _write_session_dir(sessions_path, _sid("inactive-old"), "2025-03-01T00:00:00")
        _write_session_dir(sessions_path, _sid("inactive-new"), "2025-04-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("active-old"))
        _mark_active(sm, _sid("active-new"))

        assert _ids(sm) == [
            _sid("active-new"),
            _sid("active-old"),
            _sid("inactive-new"),
            _sid("inactive-old"),
        ]

    def test_workspace_does_not_affect_order(self, tmp_path: Path):
        # 同一组时间戳，只把 workspace 调换：顺序随之改变就说明 workspace 仍在排序。
        left, right = tmp_path / "left", tmp_path / "right"
        for path, (a_ws, b_ws) in ((left, ("/a", "/b")), (right, ("/b", "/a"))):
            _write_session_dir(
                path, _sid("active-old"), "2025-01-01T00:00:00", workspace=a_ws
            )
            _write_session_dir(
                path, _sid("inactive-new"), "2025-06-01T00:00:00", workspace=b_ws
            )

        first, second = _make_manager(left), _make_manager(right)
        _mark_active(first, _sid("active-old"))
        _mark_active(second, _sid("active-old"))

        assert _ids(first) == [_sid("active-old"), _sid("inactive-new")]
        assert _ids(second) == [_sid("active-old"), _sid("inactive-new")]

    def test_missing_last_interaction_falls_back_to_id_prefix(self, tmp_path: Path):
        # metadata 没有 last_interaction → 用 id 前缀 YYYYMMDD-HHMMSS。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "20250101-101010-aaaaaaaa", None)
        _write_session_dir(sessions_path, "20250601-101010-aaaaaaaa", None)
        _write_session_dir(sessions_path, "20250301-101010-aaaaaaaa", None)

        sm = _make_manager(sessions_path)

        assert _ids(sm) == [
            "20250601-101010-aaaaaaaa",
            "20250301-101010-aaaaaaaa",
            "20250101-101010-aaaaaaaa",
        ]

    def test_id_prefix_fallback_keeps_active_first(self, tmp_path: Path):
        # 回退路径同样受「活跃优先」支配：active 的 id 前缀时间更旧也仍在前面。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "20250101-101010-aaaaaaaa", None)
        _write_session_dir(sessions_path, "20250601-101010-aaaaaaaa", None)

        sm = _make_manager(sessions_path)
        _mark_active(sm, "20250101-101010-aaaaaaaa")

        assert _ids(sm) == ["20250101-101010-aaaaaaaa", "20250601-101010-aaaaaaaa"]

    def test_unparseable_timestamp_sorts_last(self, tmp_path: Path):
        # 时间与 id 前缀都取不到 → 按 0，排在所有可解析的会话之后（不抛）。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, "99999999-999999-00000002", None)
        _write_session_dir(sessions_path, "99999999-999999-00000001", "not-a-timestamp")
        _write_session_dir(sessions_path, "20250101-101010-aaaaaaaa", None)

        sm = _make_manager(sessions_path)
        result = sm.list_sessions()

        # 两个 0 分会话之间的先后由 id 兜底（并列 ≠ 交给文件系统枚举序）。
        assert [s.id for s in result] == [
            "20250101-101010-aaaaaaaa",
            "99999999-999999-00000001",
            "99999999-999999-00000002",
        ]

    def test_tied_timestamps_are_ordered_by_id(self, tmp_path: Path):
        # 完全并列（同一时间戳）时按 id 升序：顺序确定，与 store 的枚举顺序无关。
        sessions_path = tmp_path / "sessions"
        for session_id in (
            "20250101-000000-cccccccc",
            "20250101-000000-bbbbbbbb",
            "20250101-000000-aaaaaaaa",
        ):
            _write_session_dir(sessions_path, session_id, "2025-01-01T00:00:00")

        sm = _make_manager(sessions_path)

        assert _ids(sm) == [
            "20250101-000000-aaaaaaaa",
            "20250101-000000-bbbbbbbb",
            "20250101-000000-cccccccc",
        ]


class TestListLiveSessions:
    """在内存会话的轻量投影（`/api/session/live` 的素材面）。

    契约：行集 = 已加载进内存的会话（未加载 / 已逐出者不出现，而不是
    `inactive`）；status 是即时读的 live agent 状态；顺序与 `list_sessions`
    的活跃组同序（时间降序 + id 定序）；行形状只有 id / status。
    """

    def test_projects_only_loaded_sessions(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("loaded"), "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, _sid("disk-only"), "2025-06-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("loaded"), "working", last="2025-01-01T00:00:00")

        rows = sm.list_live_sessions()

        assert [row.id for row in rows] == [_sid("loaded")]
        assert rows[0].status == "working"

    def test_empty_when_nothing_is_loaded(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("disk-only"), "2025-01-01T00:00:00")

        assert _make_manager(sessions_path).list_live_sessions() == []

    def test_every_live_status_is_projected(self, tmp_path: Path):
        sessions_path = tmp_path / "sessions"
        for name in (_sid("idle"), _sid("working"), _sid("waiting")):
            _write_session_dir(sessions_path, name, "2025-01-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("idle"), "idle", last="2025-01-01T00:00:00")
        _mark_active(sm, _sid("working"), "working", last="2025-01-02T00:00:00")
        _mark_active(sm, _sid("waiting"), "waiting", last="2025-01-03T00:00:00")

        assert {row.id: row.status for row in sm.list_live_sessions()} == {
            _sid("idle"): "idle",
            _sid("working"): "working",
            _sid("waiting"): "waiting",
        }

    def test_order_matches_the_list_active_group(self, tmp_path: Path):
        # live 与 list 活跃组的**交集**同序（本用例的会话都能被 list 看见，
        # 因此两者相等）；超集关系与"多出行"由 probe 场景钉住。
        sessions_path = tmp_path / "sessions"
        _write_session_dir(sessions_path, _sid("live-old"), "2025-01-01T00:00:00")
        _write_session_dir(sessions_path, _sid("live-new"), "2025-06-01T00:00:00")
        _write_session_dir(sessions_path, _sid("disk-newer"), "2025-07-01T00:00:00")

        sm = _make_manager(sessions_path)
        _mark_active(sm, _sid("live-old"), "idle", last="2025-01-01T00:00:00")
        _mark_active(sm, _sid("live-new"), "working", last="2025-06-01T00:00:00")

        active = [s.id for s in sm.list_sessions() if s.status != "inactive"]

        assert (
            [row.id for row in sm.list_live_sessions()]
            == active
            == [
                _sid("live-new"),
                _sid("live-old"),
            ]
        )

    def test_ordering_falls_back_to_id_prefix(self, tmp_path: Path):
        # 内存态缺 last_interaction → 与 list 同款回退（id 前缀 YYYYMMDD-HHMMSS）。
        sessions_path = tmp_path / "sessions"
        for session_id in (
            "20250101-101010-aaaaaaaa",
            "20250601-101010-aaaaaaaa",
            "20250301-101010-aaaaaaaa",
        ):
            _write_session_dir(sessions_path, session_id, None)

        sm = _make_manager(sessions_path)
        for session_id in (
            "20250101-101010-aaaaaaaa",
            "20250601-101010-aaaaaaaa",
            "20250301-101010-aaaaaaaa",
        ):
            _mark_active(sm, session_id, "idle")

        assert [row.id for row in sm.list_live_sessions()] == [
            "20250601-101010-aaaaaaaa",
            "20250301-101010-aaaaaaaa",
            "20250101-101010-aaaaaaaa",
        ]

    def test_tied_timestamps_are_ordered_by_id(self, tmp_path: Path):
        # 完全并列（同一时间戳）时按 id 升序（全序，与 list 同一 tie-break）。
        # 会话只存在于内存（store 目录是空的）——投影不触盘，照常出行。
        sm = _make_manager(tmp_path / "sessions")
        for session_id in (
            "20250101-000000-cccccccc",
            "20250101-000000-bbbbbbbb",
            "20250101-000000-aaaaaaaa",
        ):
            _mark_active(sm, session_id, "idle", last="2025-01-01T00:00:00")

        assert [row.id for row in sm.list_live_sessions()] == [
            "20250101-000000-aaaaaaaa",
            "20250101-000000-bbbbbbbb",
            "20250101-000000-cccccccc",
        ]

    def test_rows_carry_id_and_status_only(self):
        # 行形状是契约：加字段会改变所有消费方的解码面（轻量投影的意义就在这）。
        from wing.event import LiveSessionInfo

        assert set(LiveSessionInfo.model_fields) == {"id", "status"}
