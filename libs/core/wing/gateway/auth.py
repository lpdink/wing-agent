# wing/gateway/auth.py — API Key 鉴权

"""Gateway API Key 鉴权模块。

提供：
  - extract_key_from_headers: 从 HTTP headers 提取 API key
  - extract_key_from_ws: 从 WebSocket 连接提取 API key（headers + query param）
  - AuthMiddleware: Starlette HTTP 中间件，拦截未鉴权请求

鉴权逻辑完全在 Gateway 层，不侵入 Runtime。
AuthMiddleware 不缓存 AuthConfig——每次请求从 ``app.state.server.auth_config``
动态读取，确保 ``/api/system/reload`` 热重载后立即生效。
"""

from __future__ import annotations

from collections.abc import Mapping

from fastapi import WebSocket
from starlette.middleware.base import BaseHTTPMiddleware, RequestResponseEndpoint
from starlette.requests import Request
from starlette.responses import JSONResponse, Response
from starlette.types import ASGIApp

from wing.gateway.protocol import error_response

# 免鉴权路径——无论 auth.enabled 如何，这些路径始终开放。
EXEMPT_PATHS: set[str] = {"/api/health"}


def _unauthorized() -> JSONResponse:
    """构造 401 响应（每次新建，避免共享实例被 middleware 链 mutate）。

    经 protocol.error_response 输出统一 ErrorResponse 形状，使鉴权失败也能被
    wing-api-client 结构化解析（中间件在 ExceptionMiddleware 之外，无法依赖
    gateway 的 exception handler）。
    """
    return error_response(401, "Invalid or missing API key")


def extract_key_from_headers(headers: Mapping[str, str]) -> str | None:
    """从 HTTP headers 提取 API key。

    优先级：Authorization: Bearer <key> > X-API-Key: <key>。
    header name 查找不区分大小写（Starlette Headers 本身即如此）。
    """
    # 1. Authorization: Bearer <key>
    auth = headers.get("authorization", "")
    if auth.lower().startswith("bearer "):
        token = auth[7:].strip()
        if token:
            return token

    # 2. X-API-Key: <key>
    api_key = headers.get("x-api-key", "")
    if api_key:
        return api_key

    return None


def extract_key_from_ws(ws: WebSocket) -> str | None:
    """从 WebSocket 连接提取 API key。

    优先级：headers（同 HTTP）> query param ``api_key``。
    """
    # 1. Headers
    key = extract_key_from_headers(ws.headers)
    if key:
        return key

    # 2. Query parameter
    return ws.query_params.get("api_key") or None


class AuthMiddleware(BaseHTTPMiddleware):
    """HTTP API Key 鉴权中间件。

    当 ``auth_config.enabled`` 为 True 时，拦截所有非免鉴权路径的
    HTTP 请求，校验 API Key。校验通过后将 role 写入
    ``request.state.api_key_role``。

    不缓存 AuthConfig——每次 dispatch 从 ``app.state.server.auth_config``
    读取最新配置，热重载后立即生效。
    """

    def __init__(self, app: ASGIApp) -> None:
        super().__init__(app)

    async def dispatch(
        self, request: Request, call_next: RequestResponseEndpoint
    ) -> Response:
        auth_config = request.app.state.server.auth_config

        if not auth_config.enabled:
            return await call_next(request)

        if request.url.path in EXEMPT_PATHS:
            return await call_next(request)

        key = extract_key_from_headers(request.headers)
        if key is None:
            return _unauthorized()

        role = auth_config.verify(key)
        if role is None:
            return _unauthorized()

        request.state.api_key_role = role
        return await call_next(request)
