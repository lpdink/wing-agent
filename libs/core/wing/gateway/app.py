# wing_gateway/app.py — FastAPI App 工厂

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
from starlette.middleware.cors import CORSMiddleware
from starlette.responses import JSONResponse

from wing.common.logger import install_loop_exception_logger
from wing.gateway.auth import AuthMiddleware
from wing.gateway.openapi import OPENAPI_METADATA
from wing.gateway.protocol import error_response
from wing.gateway.routes import register_routes

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer


def _cors_origins(server: "GatewayServer") -> list[str]:
    """读取 `gateway.cors_origins`（App 创建时快照）。

    热重载不改变已装配的中间件栈——`cors_origins` 改了要重启网关；`static_dir`
    相反（逐请求解析，热重载即时生效）。类型不符 / 空白项静默忽略：配置对象在
    测试里常是替身，这里不能因为"没有这个键"就把服务起崩。
    """
    raw = getattr(server.gateway_config, "cors_origins", None)
    if not isinstance(raw, list | tuple):
        return []
    return [item.strip() for item in raw if isinstance(item, str) and item.strip()]


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

    app = FastAPI(
        title=OPENAPI_METADATA["title"],
        description=OPENAPI_METADATA["description"],
        version=OPENAPI_METADATA["version"],
        servers=OPENAPI_METADATA["servers"],
        openapi_tags=OPENAPI_METADATA["tags"],
        lifespan=lifespan,
    )

    app.state.server = server
    app.add_middleware(AuthMiddleware)
    # CORS 必须**后加**（Starlette 的中间件栈后加的更外层）：预检请求
    # （OPTIONS + Origin/Access-Control-Request-Method）不带鉴权头，必须在
    # AuthMiddleware 之外被应答，否则 auth 开启时开发期跨源全线 401。
    cors_origins = _cors_origins(server)
    if cors_origins:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=cors_origins,
            allow_methods=["*"],
            allow_headers=["*"],
            # 鉴权走显式请求头（Authorization / X-API-Key），不用 Cookie：
            # 不需要 credentials（开了反而要求精确 origin 并带上用户凭证）。
            allow_credentials=False,
        )
    _register_error_handlers(app)
    register_routes(app, server)

    return app
