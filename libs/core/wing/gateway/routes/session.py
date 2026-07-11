# wing_gateway/routes/session.py — Session HTTP 端点

"""Session 管理的 9 个 HTTP 端点。

所有端点通过 app.state.server.runtime 访问 WingRuntime service 层。
subscribe/unsubscribe 需要 X-Client-Id header。
"""

from __future__ import annotations

import uuid

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request

from typing import TYPE_CHECKING

from wing.gateway.protocol import (
    CreateSessionRequest,
    CreateSessionResponse,
    ForkSessionRequest,
    ForkSessionResponse,
    OkResponse,
    ResumeSessionRequest,
    ResumeSessionResponse,
    SendMessageRequest,
    SendMessageResponse,
    SessionGetResponse,
    SessionListResponse,
    SubscribeRequest,
    UnsubscribeRequest,
)

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

router = APIRouter(tags=["session"])


# ============================================================
# 依赖注入
# ============================================================


def _require_client_id(x_client_id: str | None = Header(None)) -> str:
    """从 X-Client-Id header 提取 client_id，缺失时返回 400。"""
    if x_client_id is None:
        raise HTTPException(status_code=400, detail="missing X-Client-Id header")
    return x_client_id


def _get_server(request: Request) -> GatewayServer:
    """从 app.state 获取 GatewayServer 实例。"""
    return request.app.state.server


# ============================================================
# Session 生命周期
# ============================================================


@router.post(
    "/api/session/create",
    response_model=CreateSessionResponse,
    summary="创建新 session",
)
async def create_session(
    body: CreateSessionRequest,
    request: Request,
) -> CreateSessionResponse:
    """创建新 session。

    可选指定模板名称和工作目录。如果不指定模板，使用默认模板。
    返回新创建的 session ID、模板名称和工作目录。
    """
    server = _get_server(request)
    try:
        session = server.runtime.create_session(
            template_name=body.template_name,
            workspace=body.workspace,
            agent_override=body.agent,
        )
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return CreateSessionResponse(
        session_id=session.session_id,
        template_name=session.template_name or "",
        workspace=session.session_workspace,
    )


@router.post(
    "/api/session/resume",
    response_model=ResumeSessionResponse,
    summary="恢复已有 session",
)
async def resume_session(
    body: ResumeSessionRequest,
    request: Request,
) -> ResumeSessionResponse:
    """从磁盘恢复已有 session。

    通过 session_id 加载之前持久化的 session 状态和消息历史。
    session 不存在时返回 404。
    """
    server = _get_server(request)
    try:
        session = server.runtime.resume_session(body.session_id)
    except ValueError:
        raise HTTPException(status_code=404, detail="session not found")
    return ResumeSessionResponse(
        session_id=session.session_id,
        template_name=session.template_name,
        workspace=session.session_workspace,
    )


@router.post(
    "/api/session/fork",
    response_model=ForkSessionResponse,
    summary="分叉 session",
)
async def fork_session(
    body: ForkSessionRequest,
    request: Request,
) -> ForkSessionResponse:
    """从指定 session 的指定消息处分叉出新 session。

    `source_session_id` 为源 session，`target_uuid` 为分叉点消息的 UUID。
    新 session 继承分叉点之前的所有消息历史。
    返回新 session ID 和分叉点的 draft 消息（如果有）。
    """
    server = _get_server(request)
    try:
        new_session, draft = server.runtime.fork_session(
            source_session_id=body.source_session_id,
            target_uuid=body.target_uuid,
        )
    except ValueError as e:
        error_msg = str(e)
        # 区分 session not found 和 uuid not found
        if "session" in error_msg.lower() and "not found" in error_msg.lower():
            raise HTTPException(status_code=404, detail="session not found")
        if "uuid" in error_msg.lower() or "invalid" in error_msg.lower():
            raise HTTPException(status_code=404, detail="uuid not found")
        raise HTTPException(status_code=404, detail=error_msg)
    return ForkSessionResponse(
        session_id=new_session.session_id,
        draft=draft,
    )


# ============================================================
# 订阅管理
# ============================================================


@router.post(
    "/api/session/subscribe",
    response_model=OkResponse,
    summary="订阅 session 事件",
)
async def subscribe(
    body: SubscribeRequest,
    request: Request,
    x_client_id: str = Depends(_require_client_id),
) -> OkResponse:
    """订阅 session 事件。

    需要 `X-Client-Id` header（从 WS 连接获取）。
    订阅后，该 session 的事件会通过 WS 推送给客户端。
    一个 client 可以订阅多个 session。
    """
    server = _get_server(request)

    # 验证 client 已连接
    if x_client_id not in server.clients:
        raise HTTPException(status_code=400, detail="client not connected")

    try:
        server.runtime.subscribe(x_client_id, body.session_id)
    except ValueError:
        raise HTTPException(status_code=404, detail="session not found")
    return OkResponse()


@router.post(
    "/api/session/unsubscribe",
    response_model=OkResponse,
    summary="取消订阅 session 事件",
)
async def unsubscribe(
    body: UnsubscribeRequest,
    request: Request,
    x_client_id: str = Depends(_require_client_id),
) -> OkResponse:
    """取消订阅 session 事件。

    需要 `X-Client-Id` header。取消后不再接收该 session 的事件推送。
    """
    server = _get_server(request)
    server.runtime.unsubscribe(x_client_id, body.session_id)
    return OkResponse()


# ============================================================
# 消息发送
# ============================================================


@router.post(
    "/api/session/send",
    response_model=SendMessageResponse,
    summary="发送消息到 session",
)
async def send_message(
    body: SendMessageRequest,
    request: Request,
) -> SendMessageResponse:
    """通过 HTTP 向活跃 session 发送消息。

    `silent` 为 true 时不触发 DeliveredEvent 和 SystemEvent。
    返回 `request_id` 用于前端关联响应。
    """
    server = _get_server(request)
    request_id = uuid.uuid4().hex

    # 验证 session 存在
    if server.runtime.get_session_state(body.session_id) is None:
        raise HTTPException(status_code=404, detail="session not found")

    await server.runtime.post(
        content=body.content,
        request_id=request_id,
        session_id=body.session_id,
        silent=body.silent,
    )
    return SendMessageResponse(ok=True, request_id=request_id)


# ============================================================
# 查询
# ============================================================


@router.get(
    "/api/session/list",
    response_model=SessionListResponse,
    summary="列出所有 session",
)
async def list_sessions(request: Request) -> SessionListResponse:
    """列出所有活跃 session 的摘要信息。"""
    server = _get_server(request)
    sessions = server.runtime.list_sessions()
    return SessionListResponse(sessions=sessions)


@router.get(
    "/api/session/get",
    response_model=SessionGetResponse,
    summary="获取 session 详情",
)
async def get_session(
    request: Request,
    session_id: str = Query(..., description="目标 session ID"),
) -> SessionGetResponse:
    """获取指定 session 的完整状态，包括消息历史和 agent 信息。

    session 不存在时返回 404。
    """
    server = _get_server(request)
    state = server.runtime.get_session_state(session_id)
    if state is None:
        raise HTTPException(status_code=404, detail="session not found")
    return SessionGetResponse(**state)
