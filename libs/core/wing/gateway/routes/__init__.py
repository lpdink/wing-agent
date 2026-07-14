# wing_gateway/routes/__init__.py — 路由注册

"""统一注册所有 APIRouter 到 FastAPI app。"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import FastAPI

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer


def register_routes(app: FastAPI, server: GatewayServer) -> None:
    """注册所有路由（session、system、health、ws）。"""
    from .health import router as health_router
    from .session import router as session_router
    from .system import router as system_router
    from .ws import router as ws_router

    app.include_router(health_router)
    app.include_router(session_router)
    app.include_router(system_router)
    app.include_router(ws_router)
