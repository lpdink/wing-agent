"""`notice` 事件：重试通知不再冒充错误。

背景：`with_retry` 的重试通知复用 `ErrorEvent`，而 TUI 把任何 `error` 都当作
turn 终结（`finish_turn()` + 错误单元 + 通知）——重试中的 turn 因此被前端误判
为「已经结束」。本文件钉死 notice 的事件契约（不落盘、不终结 turn）、发出它的
出口（with_retry）。
"""

from __future__ import annotations

from unittest import mock

import pytest

from wing.common.with_retry import with_retry
from wing.event import FACT_EVENTS, EVENT_TYPES, NoticeEvent, WingEvent, wire_dump

# ============================================================
# 事件契约
# ============================================================


class TestNoticeEventShape:
    def test_registered_and_not_a_fact_event(self):
        assert EVENT_TYPES["notice"] is NoticeEvent
        assert "notice" not in FACT_EVENTS, "notice 不落盘，绝不能进重放集合"

    def test_not_persisted(self):
        assert NoticeEvent.persist is False
        assert NoticeEvent(message="hi").persist is False

    def test_wire_shape(self):
        payload = wire_dump(
            NoticeEvent(
                level="warning",
                message="retrying",
                attempt=2,
                max_attempts=10,
                retry_in_s=6.0,
            )
        )
        assert payload["type"] == "notice"
        assert payload["level"] == "warning"
        assert payload["message"] == "retrying"
        assert payload["attempt"] == 2
        assert payload["max_attempts"] == 10
        assert payload["retry_in_s"] == 6.0
        assert "persist" not in payload, "persist 是 ClassVar，绝不进序列化"

    def test_wire_strips_null_fields(self):
        payload = wire_dump(NoticeEvent(message="hi"))
        assert "attempt" not in payload
        assert "retry_in_s" not in payload
        assert payload["level"] == "info"  # 默认级别


# ============================================================
# 出口：with_retry 发 notice（不再发 error）
# ============================================================


class _Host:
    """带 _config 的宿主（复刻 provider 的取参方式）+ 首调用即失败的计数。"""

    def __init__(self, max_retry_delay: float = 0.01) -> None:
        self._config = type(
            "C", (), {"max_retries": 1, "max_retry_delay": max_retry_delay}
        )()
        self.attempts = 0

    def first_call_fails(self) -> None:
        self.attempts += 1
        if self.attempts == 1:
            raise RuntimeError("boom")


class TestRetryEmitsNotice:
    @pytest.mark.asyncio
    async def test_emits_notice_with_structured_fields(self):
        from wing.event_bus import event_bus

        host = _Host(max_retry_delay=60.0)
        seen: list[WingEvent] = []
        event_bus.subscribe(seen.append)
        try:

            @with_retry(base_delay=7.0)
            async def flaky(provider):
                provider.first_call_fails()
                return "ok"

            with mock.patch("asyncio.sleep", new=mock.AsyncMock()):
                assert await flaky(host) == "ok"
        finally:
            event_bus.unsubscribe(seen.append)

        assert seen, "retry must notify the frontend"
        event = seen[0]
        assert isinstance(event, NoticeEvent), (
            f"expected NoticeEvent, got {type(event)}"
        )
        assert event.level == "warning"
        assert event.attempt == 1
        assert event.max_attempts == 1
        assert event.retry_in_s == 7.0
        assert "boom" in event.message
        assert "flaky" in event.message

    @pytest.mark.asyncio
    async def test_never_emits_error_event(self):
        from wing.event import ErrorEvent
        from wing.event_bus import event_bus

        host = _Host()
        seen: list[WingEvent] = []
        event_bus.subscribe(seen.append)
        try:

            @with_retry(base_delay=0.01)
            async def flaky(provider):
                provider.first_call_fails()
                return "ok"

            assert await flaky(host) == "ok"
        finally:
            event_bus.unsubscribe(seen.append)

        assert not [e for e in seen if isinstance(e, ErrorEvent)], (
            "重试中的失败不是 error——那是 frontend 的 turn 终结信号"
        )

    @pytest.mark.asyncio
    async def test_session_scoped(self):
        """重试通知仍是 session 作用域（由 EventBus 转成 client 定向）。"""
        from wing.event import EventTarget
        from wing.event_bus import event_bus

        host = _Host()
        seen: list[WingEvent] = []
        event_bus.subscribe(seen.append)
        try:

            @with_retry(base_delay=0.01)
            async def flaky(provider):
                provider.first_call_fails()
                return "ok"

            await flaky(host)
        finally:
            event_bus.unsubscribe(seen.append)

        target = seen[0].target
        assert isinstance(target, EventTarget)
        # EventBus 把 session 作用域解析成 client 定向（无订阅者 → 空列表）
        assert target.scope == "client"
        assert target.client_ids == []

    @pytest.mark.asyncio
    async def test_async_generator_retry_also_notifies(self):
        """流式路径（async generator）同样只发 notice。"""
        from wing.event import ErrorEvent
        from wing.event_bus import event_bus

        host = _Host()
        seen: list[WingEvent] = []
        event_bus.subscribe(seen.append)
        try:

            @with_retry(base_delay=0.01)
            async def stream(provider):
                provider.attempts += 1
                if provider.attempts == 1:
                    raise TimeoutError(
                        "stalled: no data for 120s after response header"
                    )
                yield "chunk"

            assert [c async for c in stream(host)] == ["chunk"]
        finally:
            event_bus.unsubscribe(seen.append)

        assert len(seen) == 1
        assert isinstance(seen[0], NoticeEvent)
        assert not any(isinstance(e, ErrorEvent) for e in seen)
