# wing/request_context.py
"""
wing/request_context.py — per-request 上下文

单个 ContextVar 承载 request_id / session_id / client_id，
取代 event_bus.py 中分散的三个独立 ContextVar。

设计约束：
  - 单 ContextVar：一个 RequestContext dataclass，避免 3 组 set/reset 函数
  - WingRuntime.post() 在协程开头 set，try/finally 确保 reset
  - EventBus.emit() 从中读取 request_id 和 session_id 注入事件
"""

from __future__ import annotations

import contextvars
from dataclasses import dataclass


@dataclass
class RequestContext:
    """单个请求的上下文元数据。

    request_id: 用于串联同一请求的所有事件
    session_id: 当前操作的 session（EventBus emit 时 auto-inject）
    client_id:  发起请求的客户端标识（路由表查找用）
    """

    request_id: str | None = None
    session_id: str | None = None
    client_id: str | None = None


_ctx: contextvars.ContextVar[RequestContext] = contextvars.ContextVar(
    "request_context", default=RequestContext()
)


def set_request_context(
    *,
    request_id: str | None = None,
    session_id: str | None = None,
    client_id: str | None = None,
) -> contextvars.Token:
    """设置当前协程的 RequestContext，返回 token 用于恢复。

    使用方式：
        token = set_request_context(request_id="abc", session_id="s1")
        ...  # 协程内所有 EventBus.emit() 都会自动注入这些值
        reset_request_context(token)
    """
    return _ctx.set(
        RequestContext(
            request_id=request_id,
            session_id=session_id,
            client_id=client_id,
        )
    )


def reset_request_context(token: contextvars.Token) -> None:
    """恢复 RequestContext 到 set 之前的值。"""
    _ctx.reset(token)


def get_request_context() -> RequestContext:
    """获取当前协程的 RequestContext。"""
    return _ctx.get()
