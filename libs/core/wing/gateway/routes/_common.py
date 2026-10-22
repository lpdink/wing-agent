# wing/gateway/routes/_common.py — 路由共享的机械样板

"""跨端点重复的机械件：取同一个对象、抛同一个异常。

**只收机械样板**，不收语义映射：

- 端点内 ``except LookupError: raise HTTPException(404, …)`` / ``except ValueError: 400``
  的映射留在各端点——``detail`` 文本是线格式，且有四套口径（``str(e)``、
  ``"session or uuid not found"``、``"session not found"``、逐端点自定义），
  收进全局 exception handler 会把它们统一成一套，属于协议变更。
- ``missing_session`` 也在这里：它的判据（归档 → :class:`SessionArchivedError`，
  否则普通 404）是所有「只查内存态」端点的共同结论，与上面那批 ``except`` 不同
  ——它是**构造**一个异常，不是把异常折成响应。
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING

from fastapi import Header, HTTPException, Request

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer
    from wing.session import Session


def get_server(request: Request) -> GatewayServer:
    """从 ``app.state`` 获取 GatewayServer 实例。"""
    return request.app.state.server


#: 缺 ``X-Client-Id`` 时的 400 文案（``detail`` 是线格式：改它就是改协议）。
MISSING_CLIENT_ID = "missing X-Client-Id header"


async def require_client_id(x_client_id: str | None = Header(None)) -> str:
    """``X-Client-Id`` 的 FastAPI 依赖（缺失 → 400 ``MISSING_CLIENT_ID``）。

    依赖函数住本模块：FastAPI 只读签名（参数名 / 类型 / 默认值）生成 header
    参数，位置在哪个模块不影响 ``/openapi.json``。
    """
    if x_client_id is None:
        raise HTTPException(status_code=400, detail=MISSING_CLIENT_ID)
    return x_client_id


def require_session(server: GatewayServer, session_id: str) -> Session:
    """内存态会话查询；不在内存 → :func:`missing_session` 的结论（404 / 归档）。"""
    session = server.runtime.get_session(session_id)
    if session is None:
        raise missing_session(server, session_id)
    return session


def require_session_state(server: GatewayServer, session_id: str) -> dict:
    """``require_session`` 的 ``get_session_state`` 版本（同上；返回状态字典）。"""
    state = server.runtime.get_session_state(session_id)
    if state is None:
        raise missing_session(server, session_id)
    return state


def missing_session(server: GatewayServer, session_id: str) -> Exception:
    """「内存态查询」找不到会话时的错误：归档命中 → ``SessionArchivedError``
    （app handler 映射 404 + session_archived，可行动提示），否则普通 404。

    这些端点本来只查内存态（不水合），归档会话对它们同样是"不在场"；区别只在
    错误的可行动性——裸 API 消费方要看得出"它没消失，只是被归档了"。
    """
    from wing.session import SessionArchivedError

    error = server.runtime.missing_session_error(session_id)
    if isinstance(error, SessionArchivedError):
        return error
    return HTTPException(status_code=404, detail="session not found")


def preflight_body_size(
    request: Request, max_bytes: int, message: Callable[[int, int], str]
) -> None:
    """``Content-Length`` 预检（收包**之前**）：声明的字节数超限即 400。

    只看声明值，不读 body——超限的请求一个字节都不必吞进内存。声明值与真实
    字节不符（缺头 / 说谎的头）由收包之后的字节闸门按真实字节兜底。

    ``message`` 是**文案工厂** ``(declared, max_bytes) -> detail``：两条上传
    通道各用自己的那份（``too_large_message`` / ``file_too_large_message``），
    与字节闸门共用同一句话。
    """
    declared = _declared_content_length(request)
    if declared is not None and declared > max_bytes:
        raise HTTPException(status_code=400, detail=message(declared, max_bytes))


def _declared_content_length(request: Request) -> int | None:
    """``Content-Length`` 头解析成整数（缺失 / 不可解析 = ``None``）。

    不可解析的值不在这里拒绝：声明值与真实字节不符一律由字节闸门兜底。
    """
    raw = request.headers.get("content-length")
    if raw is None:
        return None
    try:
        return int(raw)
    except ValueError:
        return None
