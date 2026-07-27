# wing_gateway/routes/tools.py — 远程工具注册端点

"""远程工具注册的 HTTP 端点。

tool host 先建立 WS 连接（声明 client_id），再经此端点注册工具。
注册要求该 client_id 已持有活跃 WS——工具的调用与结果都走那条 WS。

Route handler 只做：参数验证 → 调 RemoteToolManager → 构造响应。
异常映射：未连接 / 工具碰撞 → 400。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import APIRouter, Depends, Header, HTTPException, Request

from wing.gateway.protocol import RegisterToolsRequest, RegisterToolsResponse

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

router = APIRouter(tags=["tools"])


def _require_client_id(x_client_id: str | None = Header(None)) -> str:
    if x_client_id is None:
        raise HTTPException(status_code=400, detail="missing X-Client-Id header")
    return x_client_id


def _get_server(request: Request) -> GatewayServer:
    return request.app.state.server


@router.post(
    "/api/tools/register",
    response_model=RegisterToolsResponse,
    summary="注册远程工具",
)
async def register_tools(
    body: RegisterToolsRequest,
    request: Request,
    x_client_id: str = Depends(_require_client_id),
) -> RegisterToolsResponse:
    server = _get_server(request)
    manager = server.remote_tools

    if not manager.is_attached(x_client_id):
        raise HTTPException(
            status_code=400,
            detail=f"client '{x_client_id}' has no active WebSocket connection",
        )

    try:
        registered = manager.register_tools(x_client_id, body.tools)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))

    return RegisterToolsResponse(ok=True, registered=registered)
