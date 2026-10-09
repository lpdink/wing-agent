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

import contextlib
import contextvars
from collections.abc import Iterator
from dataclasses import dataclass, replace


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


@contextlib.contextmanager
def session_context(session_id: str) -> Iterator[None]:
    """在**当前** RequestContext 上绑定 ``session_id``（保留 request_id 等）。

    供「已知自己在为哪个 session 工作、但入站协程未设过上下文」的路径使用：
    会话逐出拆解、会话级 HTTP 端点（compact）——这些路径的日志（以及其调用
    链上的日志）借助 ContextVar 自动带上归属，不需要逐条手工拼 id。块内的
    异常（例如映射成 404 的 ``LookupError``）照常传播，退出时恢复先前上下文。

    约束：**只面向未设上下文的入站路径**。合并语义保留的 request_id 属于
    外层请求——若在**另一会话的请求作用域内**用它切换 session，日志与事件
    会带上与该会话无关的 request_id（比不标注更误导）；轮内操作（如
    ``ensure_loaded`` 命中同会话）不受影响，因为 id 本就同源。
    """
    token = _ctx.set(replace(_ctx.get(), session_id=session_id))
    try:
        yield
    finally:
        _ctx.reset(token)
