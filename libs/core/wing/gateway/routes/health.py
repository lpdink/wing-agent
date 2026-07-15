# wing_gateway/routes/health.py — 健康检查端点

"""GET /api/health — 服务健康检查。"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

from fastapi import APIRouter, Request

from wing.gateway.protocol import HealthResponse

router = APIRouter(tags=["health"])


def _get_version() -> str:
    """获取 wing-agent 版本号。"""
    try:
        return version("wing-agent")
    except PackageNotFoundError:
        return "dev"


@router.get(
    "/api/health",
    response_model=HealthResponse,
    summary="健康检查",
)
async def health(request: Request) -> HealthResponse:
    """健康检查——返回服务身份、状态、版本号和运行时长。"""
    server = request.app.state.server
    return HealthResponse(
        service="wing-gateway",
        status="ok",
        version=_get_version(),
        uptime=server.uptime,
    )
