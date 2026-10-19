"""tests/test_wake_archive_link.py — 归档联动：会话进归档区 → 相关唤醒触发器一并取消。

联动**只能**发生在 L4（``runtime.py``）：``session_manager`` 不反向依赖 ``wake``。这里钉两件事：

  - 真的归档了（``archived=True``）才取消；幂等 no-op 的归档不碰触发器；
  - 自动归档（``archive_stale_sessions``）对**每个**被收走的会话都做一次联动。
"""

from __future__ import annotations

import pytest

from wing.runtime import WingRuntime

SOURCE = "20260101-120000-source"
OTHER = "20260101-120000-other"


@pytest.fixture
def runtime(monkeypatch: pytest.MonkeyPatch):
    """真实 runtime + 记录型 wake（只替换 cancel_for_session，其余用真的）。"""
    runtime = WingRuntime()
    calls: list[str] = []

    def _record(session_id: str) -> int:
        calls.append(session_id)
        return len(calls)

    monkeypatch.setattr(runtime.wake, "cancel_for_session", _record)
    return runtime, calls


class TestArchiveSession:
    def test_archived_session_cancels_its_triggers(
        self, runtime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, calls = runtime
        monkeypatch.setattr(app.sm, "archive_session", lambda sid: (True, "archived"))
        assert app.archive_session(SOURCE) == (True, "archived")
        assert calls == [SOURCE]

    def test_noop_archive_does_not_touch_triggers(
        self, runtime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, calls = runtime
        monkeypatch.setattr(
            app.sm, "archive_session", lambda sid: (False, "nothing to archive")
        )
        assert app.archive_session(SOURCE) == (False, "nothing to archive")
        assert calls == []


class TestArchiveStale:
    @pytest.mark.asyncio
    async def test_sweep_cancels_each_archived_session(
        self, runtime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, calls = runtime

        async def _sweep() -> list[str]:
            return [SOURCE, OTHER]

        monkeypatch.setattr(app.archiver, "sweep", _sweep)
        assert await app.archive_stale_sessions() == [SOURCE, OTHER]
        assert calls == [SOURCE, OTHER]

    @pytest.mark.asyncio
    async def test_empty_sweep_does_nothing(
        self, runtime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, calls = runtime

        async def _sweep() -> list[str]:
            return []

        monkeypatch.setattr(app.archiver, "sweep", _sweep)
        assert await app.archive_stale_sessions() == []
        assert calls == []
