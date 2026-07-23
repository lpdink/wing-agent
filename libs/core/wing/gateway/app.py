# wing_gateway/app.py — FastAPI App 工厂

"""创建 FastAPI app 实例，注入元数据，注册路由。

将 app 创建从 GatewayServer 中提取为独立工厂函数，
便于测试（TestClient）和未来 daemon 模式复用。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import FastAPI

from wing.gateway.auth import AuthMiddleware
from wing.gateway.openapi import OPENAPI_METADATA
from wing.gateway.routes import register_routes

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer


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
    register_routes(app, server)

    return app
