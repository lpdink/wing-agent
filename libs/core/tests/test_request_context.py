"""Tests for wing.request_context（每请求上下文与 session 绑定）。"""

import asyncio

import pytest

from wing.request_context import (
    RequestContext,
    get_request_context,
    reset_request_context,
    session_context,
    set_request_context,
)


def test_session_context_binds_and_restores() -> None:
    """绑定 session 不丢已有字段（合并语义）；退出后恢复先前上下文。"""
    token = set_request_context(request_id="req-1", client_id="tui")
    try:
        with session_context("sid-1"):
            ctx = get_request_context()
            assert ctx.session_id == "sid-1"
            assert ctx.request_id == "req-1"
            assert ctx.client_id == "tui"
        assert get_request_context() == RequestContext(
            request_id="req-1", client_id="tui"
        )
    finally:
        reset_request_context(token)


def test_session_context_overrides_outer_session() -> None:
    token = set_request_context(session_id="outer", request_id="req-2")
    try:
        with session_context("inner"):
            assert get_request_context().session_id == "inner"
        assert get_request_context().session_id == "outer"
    finally:
        reset_request_context(token)


def test_session_context_restores_on_exception() -> None:
    with pytest.raises(RuntimeError, match="boom"):
        with session_context("sid-2"):
            raise RuntimeError("boom")

    assert get_request_context() == RequestContext()


@pytest.mark.asyncio
async def test_session_context_is_task_local() -> None:
    """绑定只影响当前任务：并发任务各持所绑（ContextVar 拷贝语义）。"""

    async def bind_and_read(session_id: str) -> str | None:
        with session_context(session_id):
            await asyncio.sleep(0)
            return get_request_context().session_id

    assert await asyncio.gather(bind_and_read("a"), bind_and_read("b")) == ["a", "b"]
    assert get_request_context().session_id is None
