# wing_gateway/routes/__init__.py — 路由注册

"""统一注册所有 APIRouter 到 FastAPI app。"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import FastAPI

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer


def register_routes(app: FastAPI, server: GatewayServer) -> None:
    """注册所有路由（session、system、health、tools、workspace、ws、static）。

    `static` 是 catch-all（静态托管 + SPA fallback）——**必须最后注册**：路由按
    注册序匹配，先注册的真实路由（含框架自带的 `/docs` 等）优先命中，只有没人
    认领的路径才落到它。
    """
    from .health import router as health_router
    from .session import router as session_router
    from .static import router as static_router
    from .system import router as system_router
    from .tools import router as tools_router
    from .workspace import router as workspace_router
    from .ws import router as ws_router

    app.include_router(health_router)
    app.include_router(session_router)
    app.include_router(system_router)
    app.include_router(tools_router)
    app.include_router(workspace_router)
    app.include_router(ws_router)
    app.include_router(static_router)
