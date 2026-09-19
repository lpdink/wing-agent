"""空闲会话逐出（session eviction）单元测试。

覆盖：

- ``evict_idle_sessions`` 的三条判定（空闲 / 无订阅 / TTL）与两条硬条件
  （后台任务、非持久后端）；
- touch（会话状态变化重置空闲计时器）与 reaper 的 EventBus 触摸通道；
- 逐出 → ``ensure_loaded`` 水合（链完整、磁盘状态不动、provider 已关闭）；
- ``release_session`` 的语义（幂等 / 钉住拒绝 / 未知会话）；
- ``BackgroundScheduler``（周期触发 / 异常隔离 / 幂等启停）。

时钟全部注入（``now=`` 参数与假时钟），不依赖真实等待时长。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from wing.background import BackgroundScheduler
from wing.event import DeliveredEvent
from wing.event_bus import event_bus
from wing.runtime import WingRuntime
from wing.schema import Message


def _runtime() -> WingRuntime:
    return WingRuntime()


def _seed(session, *contents: str) -> None:
    """向 session 追加若干 user 消息（经 CM 落盘，形成磁盘事实）。"""
    for content in contents:
        session.context_manager.add_message(Message(role="user", content=content))


def _go_working(session) -> None:
    """把会话置为 working（在飞 turn）。"""
    session.agent._working = True


def _go_waiting(session) -> None:
    """把会话置为 waiting（Ask 挂起）。"""
    session.agent._inbox._feedback_waiters["tc_stub"] = (
        asyncio.get_running_loop().create_future()
    )


class TestIdleEviction:
    """三条判定：空闲、无订阅、TTL。"""

    @pytest.mark.asyncio
    async def test_evicts_idle_session_beyond_ttl(self):
        runtime = _runtime()
        session = runtime.create_session()
        _seed(session, "hello")
        sid = session.session_id

        assert runtime.sm.evict_idle_sessions(ttl_seconds=0.0) == [sid]
        assert runtime.sm.get_session(sid) is None

        await runtime.sm.wait_teardowns()
        # 磁盘状态不动：会话仍可从磁盘水合。
        assert runtime.sm._stores["file"].exists(sid)
        reborn = runtime.sm.ensure_loaded(sid)
        assert reborn.session_id == sid
        assert [m.content for m in reborn.context_manager.get_context_window()] == [
            "hello"
        ]

    @pytest.mark.asyncio
    async def test_keeps_session_within_ttl(self):
        runtime = _runtime()
        session = runtime.create_session()
        now = time.monotonic()

        assert runtime.sm.evict_idle_sessions(ttl_seconds=100.0, now=now + 50) == []
        assert runtime.sm.get_session(session.session_id) is not None

        assert runtime.sm.evict_idle_sessions(ttl_seconds=100.0, now=now + 101) == [
            session.session_id
        ]

    @pytest.mark.asyncio
    async def test_touch_resets_idle_timer(self):
        runtime = _runtime()
        session = runtime.create_session()
        sid = session.session_id

        # 把空闲起点推到 10000s 前：已远超 TTL
        runtime.sm._last_active[sid] = time.monotonic() - 10_000
        # 状态变化（touch）→ 计时器归零 → 不再可逐出
        runtime.sm.touch(sid)
        assert runtime.sm.evict_idle_sessions(ttl_seconds=100.0) == []
        assert runtime.sm.get_session(sid) is not None

    @pytest.mark.asyncio
    async def test_working_and_waiting_are_pinned(self):
        runtime = _runtime()
        working = runtime.create_session()
        waiting = runtime.create_session()
        _go_working(working)
        _go_waiting(waiting)

        assert runtime.sm.evict_idle_sessions(ttl_seconds=0.0) == []
        assert runtime.sm.get_session(working.session_id) is not None
        assert runtime.sm.get_session(waiting.session_id) is not None

    @pytest.mark.asyncio
    async def test_subscribed_session_is_pinned(self):
        runtime = _runtime()
        session = runtime.create_session()
        sid = session.session_id
        event_bus.route_attach("client-x", sid)
        try:
            assert runtime.sm.evict_idle_sessions(ttl_seconds=0.0) == []
            assert runtime.sm.get_session(sid) is not None
        finally:
            event_bus.route_detach("client-x", sid)

        assert runtime.sm.evict_idle_sessions(ttl_seconds=0.0) == [sid]

    @pytest.mark.asyncio
    async def test_non_durable_backend_is_pinned(self):
        """memory 后端逐出 = 数据销毁：永远不逐出。"""
        runtime = _runtime()
        session = runtime.create_session(backend="memory")
        _seed(session, "ephemeral")

        assert runtime.sm.evict_idle_sessions(ttl_seconds=0.0) == []
        assert runtime.sm.get_session(session.session_id) is not None

    @pytest.mark.asyncio
    async def test_background_work_is_pinned(self):
        runtime = _runtime()
        session = runtime.create_session()
        gate = asyncio.Event()
        task = asyncio.create_task(gate.wait())
        session.agent.register_background(task)

        assert runtime.sm.evict_idle_sessions(ttl_seconds=0.0) == []

        gate.set()
        await task
        await asyncio.sleep(0)  # done callback 注销登记
        assert runtime.sm.evict_idle_sessions(ttl_seconds=0.0) == [session.session_id]

    @pytest.mark.asyncio
    async def test_evict_is_idempotent(self):
        runtime = _runtime()
        session = runtime.create_session()
        assert runtime.sm.evict(session.session_id, reason="test") is True
        assert runtime.sm.evict(session.session_id, reason="test") is False
        await runtime.sm.wait_teardowns()

    @pytest.mark.asyncio
    async def test_teardown_closes_worker_and_providers(self):
        from wing.provider.openai_compat import OpenAICompatProvider

        runtime = _runtime()
        session = runtime.create_session()
        provider = session.agent.model_provider
        assert isinstance(provider, OpenAICompatProvider)

        runtime.sm.evict(session.session_id, reason="test")
        await runtime.sm.wait_teardowns()

        assert session.agent._providers == {}
        assert provider._client.is_closed is True
        assert session.agent._worker.done() is True


class TestReaper:
    """SessionReaper：触摸通道 + sweep 策略。"""

    @pytest.mark.asyncio
    async def test_attach_is_idempotent_and_touches_on_event(self):
        runtime = _runtime()
        session = runtime.create_session()
        sid = session.session_id
        reaper = runtime.reaper

        reaper.attach()
        reaper.attach()  # 幂等
        try:
            runtime.sm._last_active[sid] = time.monotonic() - 10_000
            event_bus.emit(DeliveredEvent(session_id=sid))
            assert time.monotonic() - runtime.sm._last_active[sid] < 5
        finally:
            reaper.detach()
            reaper.detach()  # 幂等

    @pytest.mark.asyncio
    async def test_sweep_respects_ttl_and_enabled(self):
        runtime = _runtime()
        session = runtime.create_session()
        sid = session.session_id

        from wing.config import get_config

        config = get_config()
        original = config.sessions.eviction.idle_ttl_seconds
        config.sessions.eviction.idle_ttl_seconds = 0.001
        try:
            await asyncio.sleep(0.01)
            assert await runtime.reap_idle_sessions() == [sid]
        finally:
            config.sessions.eviction.idle_ttl_seconds = original

        # enabled=False → 扫描不做事（新会话不再被逐出）。
        session2 = runtime.create_session()
        config.sessions.eviction.enabled = False
        try:
            await asyncio.sleep(0.01)
            assert await runtime.reap_idle_sessions() == []
            assert runtime.sm.get_session(session2.session_id) is not None
        finally:
            config.sessions.eviction.enabled = True


class TestRelease:
    """release_session：显式逐出的语义（幂等 / 钉住拒绝 / 未知会话）。"""

    @pytest.mark.asyncio
    async def test_release_loaded_session(self):
        runtime = _runtime()
        session = runtime.create_session()
        _seed(session, "hello")
        sid = session.session_id

        released, detail = runtime.release_session(sid)
        assert (released, detail) == (True, "released")
        assert runtime.sm.get_session(sid) is None

        await runtime.sm.wait_teardowns()
        # 幂等：不在内存但磁盘存在 → (False, "not loaded")。
        assert runtime.release_session(sid) == (False, "not loaded")

    @pytest.mark.asyncio
    async def test_release_unknown_session_raises_lookup_error(self):
        runtime = _runtime()
        with pytest.raises(LookupError):
            runtime.release_session("20260101-000000-nope")

    @pytest.mark.asyncio
    async def test_release_refuses_busy_session(self):
        runtime = _runtime()
        session = runtime.create_session()
        _go_working(session)

        with pytest.raises(RuntimeError, match="status=working"):
            runtime.release_session(session.session_id)
        assert runtime.sm.get_session(session.session_id) is not None

    @pytest.mark.asyncio
    async def test_release_refuses_subscribed_session(self):
        runtime = _runtime()
        session = runtime.create_session()
        sid = session.session_id
        event_bus.route_attach("client-x", sid)
        try:
            with pytest.raises(RuntimeError, match="subscribed"):
                runtime.release_session(sid)
        finally:
            event_bus.route_detach("client-x", sid)
        assert runtime.sm.get_session(sid) is not None

    @pytest.mark.asyncio
    async def test_release_refuses_non_durable_session(self):
        runtime = _runtime()
        session = runtime.create_session(backend="memory")
        with pytest.raises(RuntimeError, match="non-durable"):
            runtime.release_session(session.session_id)
        assert runtime.sm.get_session(session.session_id) is not None


class TestHydration:
    """逐出后的按需水合入口。"""

    @pytest.mark.asyncio
    async def test_ensure_loaded_rehydrates_chain_and_model_record(self):
        runtime = _runtime()
        session = runtime.create_session()
        _seed(session, "alpha", "beta")
        sid = session.session_id
        session._apply_model("gpt-4", "alt")  # 落盘模型记录（快照语义）

        runtime.sm.evict(sid, reason="test")
        await runtime.sm.wait_teardowns()

        reborn = runtime.sm.ensure_loaded(sid)
        assert reborn.session_id == sid
        assert reborn.agent.model == "gpt-4"
        assert reborn.agent.model_provider.name == "alt"
        assert [m.content for m in reborn.context_manager.get_context_window()] == [
            "alpha",
            "beta",
        ]

    @pytest.mark.asyncio
    async def test_ensure_loaded_unknown_session_raises(self):
        runtime = _runtime()
        with pytest.raises(LookupError):
            runtime.sm.ensure_loaded("20260101-000000-nope")

    @pytest.mark.asyncio
    async def test_blank_session_is_not_rehydratable(self):
        """空会话（无消息 → 无磁盘痕迹）逐出后即不可恢复。"""
        runtime = _runtime()
        session = runtime.create_session()
        sid = session.session_id

        runtime.sm.evict(sid, reason="test")
        await runtime.sm.wait_teardowns()
        with pytest.raises(LookupError):
            runtime.sm.ensure_loaded(sid)

    @pytest.mark.asyncio
    async def test_runtime_subscribe_hydrates_evicted_session(self):
        runtime = _runtime()
        session = runtime.create_session()
        _seed(session, "hello")
        sid = session.session_id

        runtime.sm.evict(sid, reason="test")
        await runtime.sm.wait_teardowns()

        runtime.subscribe("client-x", sid)
        try:
            assert runtime.sm.get_session(sid) is not None
        finally:
            runtime.unsubscribe("client-x", sid)


class TestBackgroundScheduler:
    """周期任务宿主：触发 / 异常隔离 / 幂等启停。"""

    @pytest.mark.asyncio
    async def test_job_runs_periodically(self):
        calls: list[int] = []

        async def job() -> None:
            calls.append(1)

        scheduler = BackgroundScheduler()
        scheduler.add_job("tick", 0.01, job)
        scheduler.start()
        await asyncio.sleep(0.05)
        await scheduler.stop()
        assert len(calls) >= 1
        assert scheduler.jobs == ["tick"]
        assert scheduler.running is False
        await scheduler.stop()  # 幂等

    @pytest.mark.asyncio
    async def test_job_failure_is_isolated(self):
        calls: list[str] = []

        async def boom() -> None:
            calls.append("boom")
            raise RuntimeError("job exploded")

        def ok() -> None:
            calls.append("ok")

        scheduler = BackgroundScheduler()
        scheduler.add_job("boom", 0.01, boom)
        scheduler.add_job("ok", 0.01, ok)
        scheduler.start()
        await asyncio.sleep(0.05)
        assert scheduler.running is True  # 循环没被 job 异常带崩
        await scheduler.stop()
        assert "boom" in calls and "ok" in calls

    def test_add_job_validates(self):
        scheduler = BackgroundScheduler()
        scheduler.add_job("a", 1.0, lambda: None)
        with pytest.raises(ValueError, match="already registered"):
            scheduler.add_job("a", 1.0, lambda: None)
        with pytest.raises(ValueError, match="interval"):
            scheduler.add_job("b", 0.0, lambda: None)
        scheduler.remove_job("a")
        assert scheduler.jobs == []
