# wing_gateway/app.py — FastAPI App 工厂

"""创建 FastAPI app 实例，注入元数据，注册路由。

将 app 创建从 GatewayServer 中提取为独立工厂函数，
便于测试（TestClient）和未来 daemon 模式复用。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from starlette.exceptions import HTTPException as StarletteHTTPException
from starlette.responses import JSONResponse

from wing.gateway.auth import AuthMiddleware
from wing.gateway.openapi import OPENAPI_METADATA
from wing.gateway.protocol import error_response
from wing.gateway.routes import register_routes

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer


def _register_error_handlers(app: FastAPI) -> None:
    """统一所有错误响应为 protocol.ErrorResponse 形状。

    - 注册在 starlette 的 HTTPException 基类上，同时覆盖 FastAPI 路由抛出的
      HTTPException（子类，400/404/500/504）与 router 层的 405/未知路由（父类）。
    - 单独处理请求体校验失败（RequestValidationError，422，非 HTTPException）。
    - 鉴权拒绝（401）由 AuthMiddleware 直接经 protocol.error_response 输出——
      中间件位于 ExceptionMiddleware 之外，抛异常不会被这里捕获。
    """

    @app.exception_handler(StarletteHTTPException)
    async def _on_http_exception(
        request: Request, exc: StarletteHTTPException
    ) -> JSONResponse:
        detail = None if exc.detail is None else str(exc.detail)
        return error_response(exc.status_code, detail, headers=exc.headers)

    @app.exception_handler(RequestValidationError)
    async def _on_validation_error(
        request: Request, exc: RequestValidationError
    ) -> JSONResponse:
        detail = "; ".join(str(e.get("msg", "invalid")) for e in exc.errors())
        return error_response(422, detail or "validation error")


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
