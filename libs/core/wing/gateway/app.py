# wing_gateway/app.py — FastAPI App 工厂

"""创建 FastAPI app 实例，注入元数据，注册路由。

将 app 创建从 GatewayServer 中提取为独立工厂函数，
便于测试（TestClient）和未来 daemon 模式复用。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse

from wing.gateway.auth import AuthMiddleware
from wing.gateway.openapi import OPENAPI_METADATA
from wing.gateway.protocol import ErrorResponse
from wing.gateway.routes import register_routes

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

# HTTP 状态码 → ErrorResponse.error 类型。未列出的回退到 "error"。
_HTTP_ERROR_TYPES: dict[int, str] = {
    400: "bad_request",
    401: "unauthorized",
    403: "forbidden",
    404: "not_found",
    409: "conflict",
}


def _register_error_handlers(app: FastAPI) -> None:
    """统一错误响应形状为 protocol.ErrorResponse。

    FastAPI 默认把 HTTPException 渲染成 ``{"detail": ...}``，与
    wing-api-client 期望的 ``ErrorResponse``（含 ``error`` 字段）不一致，
    导致客户端反序列化恒为 None、只能回退到原始文本。这里改为输出
    ErrorResponse，使前后端错误契约一致且结构化错误真正生效。
    """

    @app.exception_handler(HTTPException)
    async def _on_http_exception(request: Request, exc: HTTPException) -> JSONResponse:
        body = ErrorResponse(
            error=_HTTP_ERROR_TYPES.get(exc.status_code, "error"),
            detail=None if exc.detail is None else str(exc.detail),
        )
        return JSONResponse(
            status_code=exc.status_code,
            content=body.model_dump(exclude_none=True),
            headers=getattr(exc, "headers", None),
        )


def create_app(server: GatewayServer) -> FastAPI:
    """创建 FastAPI app 并注册所有路由。

    Args:
        server: GatewayServer 实例，通过 app.state 注入给 routes

    Returns:
        配置好的 FastAPI app
    """
    app = FastAPI(
        title=OPENAPI_METADATA["title"],
        description=OPENAPI_METADATA["description"],
        version=OPENAPI_METADATA["version"],
        servers=OPENAPI_METADATA["servers"],
        openapi_tags=OPENAPI_METADATA["tags"],
    )

    app.state.server = server
    app.add_middleware(AuthMiddleware)
    _register_error_handlers(app)
    register_routes(app, server)

    return app
