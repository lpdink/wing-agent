# wing_gateway/routes/health.py — 健康检查端点

"""GET /api/health — 服务健康检查。"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

from fastapi import APIRouter, Request

from wing.build_info import get_commit, get_version
from wing.gateway.protocol import HealthResponse

router = APIRouter(tags=["health"])


def _get_version() -> str:
    """网关版本号：构建信息（构建时注入）→ 发行包元数据 → dev。

    构建信息是主路径：发版 wheel 与 ``pip install libs/core`` 的开发环境
    都能拿到网关自身构建时的版本（元数据仅在旧安装缺生成文件时兜底）。
    """
    if (build_version := get_version()) is not None:
        return build_version
    for dist in ("wing-agent", "wing-gateway"):
        try:
            return version(dist)
        except PackageNotFoundError:
            continue
    return "dev"


@router.get(
    "/api/health",
    response_model=HealthResponse,
    summary="健康检查",
)
async def health(request: Request) -> HealthResponse:
    """健康检查——返回服务身份、状态、版本、commit hash 和运行时长。"""
    server = request.app.state.server
    return HealthResponse(
        service="wing-gateway",
        status="ok",
        version=_get_version(),
        commit=get_commit(),
        uptime=server.uptime,
    )
