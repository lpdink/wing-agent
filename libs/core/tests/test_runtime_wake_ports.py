"""tests/test_runtime_wake_ports.py — 组合根注入给 ``wake`` 的窄接口（生产实现）。

``_RuntimeWakePorts.session_will_run_again`` 是 ``done`` 静息判据唯一读的运行时事实
（``wing/wake/quiescence.py``）。它的两个非平凡分支各钉一条：未加载（逐出 / 从未水合）算
"不会再动"，而**排队输入**必须单独看——消息已入队、worker 还没开始下一轮时
``status`` 仍是 ``idle``，但那一轮一定会跑。
"""

from __future__ import annotations

from typing import Any, cast

from wing.runtime import WingRuntime, _RuntimeWakePorts


class _FakeAgent:
    def __init__(self, *, pending_input: bool) -> None:
        self.has_pending_input = pending_input


class _FakeSession:
    def __init__(self, *, status: str, pending_input: bool = False) -> None:
        self.status = status
        self.agent = _FakeAgent(pending_input=pending_input)


class _FakeRuntime:
    def __init__(self, sessions: dict[str, _FakeSession]) -> None:
        self._sessions = sessions

    def get_session(self, session_id: str) -> _FakeSession | None:
        return self._sessions.get(session_id)


def _ports(sessions: dict[str, _FakeSession]) -> _RuntimeWakePorts:
    fake = cast(Any, _FakeRuntime(sessions))
    return _RuntimeWakePorts(cast(WingRuntime, fake))


class TestSessionWillRunAgain:
    def test_an_unloaded_session_will_not_run_again(self) -> None:
        """逐出 / 从未水合的会话读不到 → 不会再动（判据据此判静息）。"""
        assert _ports({}).session_will_run_again("20260101-120000-ghost") is False

    def test_an_idle_session_will_not_run_again(self) -> None:
        assert (
            _ports({"s": _FakeSession(status="idle")}).session_will_run_again("s")
            is False
        )

    def test_a_working_session_will_run_again(self) -> None:
        assert (
            _ports({"s": _FakeSession(status="working")}).session_will_run_again("s")
            is True
        )

    def test_a_waiting_session_will_run_again(self) -> None:
        """``waiting`` = 卡在一个提问上，答了它就接着跑。"""
        assert (
            _ports({"s": _FakeSession(status="waiting")}).session_will_run_again("s")
            is True
        )

    def test_queued_input_counts_even_while_idle(self) -> None:
        """排队输入是 ``idle`` 之外的第二条"还会动"的证据（信封已入队、下一轮还没开始）。"""
        session = _FakeSession(status="idle", pending_input=True)
        assert _ports({"s": session}).session_will_run_again("s") is True
