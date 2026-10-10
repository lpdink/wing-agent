# wing/gateway/app.py — FastAPI App 工厂

"""创建 FastAPI app 实例，注入元数据，注册路由。

将 app 创建从 GatewayServer 中提取为独立工厂函数，
便于测试（TestClient）和未来 daemon 模式复用。
"""

from __future__ import annotations

from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import TYPE_CHECKING

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from starlette.exceptions import HTTPException as StarletteHTTPException
from starlette.responses import JSONResponse

from wing.common.logger import install_loop_exception_logger
from wing.gateway.auth import AuthMiddleware
from wing.gateway.openapi import OPENAPI_METADATA
from wing.gateway.protocol import error_response
from wing.gateway.routes import register_routes
from wing.gateway.setup_guard import (
    SetupGuardMiddleware,
    SetupModeError,
    render_setup_detail,
)

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer


def _register_error_handlers(app: FastAPI) -> None:
    """统一所有错误响应为 protocol.ErrorResponse 形状。

    - 注册在 starlette 的 HTTPException 基类上，同时覆盖 FastAPI 路由抛出的
      HTTPException（子类，400/404/500/504）与 router 层的 405/未知路由（父类）。
    - 单独处理请求体校验失败（RequestValidationError，422，非 HTTPException）。
    - 鉴权拒绝（401）由 AuthMiddleware 直接经 protocol.error_response 输出——
      中间件位于 ExceptionMiddleware 之外，抛异常不会被这里捕获。
    - setup mode 下访问 runtime（SetupModeError）也要有 503 + error="setup_mode"
      的形状：守门中间件已经保证这类访问不可达，这里是万一可达时的安全网
      （也让 Rust 侧的 is_setup_mode() 拿到正确的协议形状）。
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

    @app.exception_handler(SetupModeError)
    async def _on_setup_mode(request: Request, exc: SetupModeError) -> JSONResponse:
        # 显式覆盖 error 码（P4）：HTTP_ERROR_TYPES[503] 是通用的 service_unavailable。
        return error_response(
            503, render_setup_detail(exc.problems), error="setup_mode"
        )


def create_app(server: GatewayServer) -> FastAPI:
    """创建 FastAPI app 并注册所有路由。

    Args:
        server: GatewayServer 实例，通过 app.state 注入给 routes

    Returns:
        配置好的 FastAPI app
    """

    @asynccontextmanager
    async def lifespan(_app: FastAPI) -> AsyncIterator[None]:
        """进程生命周期钩子：启停后台周期任务（uvicorn 自动调用）。

        不启 lifespan（如无上下文的 TestClient）时后台任务完全不跑——
        单测与嵌入式使用零副作用。
        """
        install_loop_exception_logger()
        server.start_background()
        try:
            yield
        finally:
            await server.stop_background()
            # provider 池是全进程共享资源，生命周期随进程：lifespan 收尾
            # （uvicorn 优雅停机，含 /api/shutdown → SIGTERM 路径）是它的
            # 终结入口——关掉全部 client，释放 keepalive socket。
            from wing.provider.pool import close_providers

            await close_providers()

    app = FastAPI(
        title=OPENAPI_METADATA["title"],
        description=OPENAPI_METADATA["description"],
        version=OPENAPI_METADATA["version"],
        servers=OPENAPI_METADATA["servers"],
        openapi_tags=OPENAPI_METADATA["tags"],
        lifespan=lifespan,
    )

    app.state.server = server
    # 中间件顺序（Starlette：``add_middleware`` 是 ``user_middleware.insert(0, …)``，
    # 即**后 add 的在外层**）。AuthMiddleware 必须在最外层：修复模式的 loopback
    # 判定要**先**发生——setup mode 下非 loopback 的任何路径都先吃 403，
    # loopback 才轮到守门中间件决定「放行 or 503」（总设计 §8.4）。
    app.add_middleware(SetupGuardMiddleware)
    app.add_middleware(AuthMiddleware)
    _register_error_handlers(app)
    register_routes(app)

    return app
