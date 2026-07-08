# wing_gateway/routes/session.py — Session HTTP 端点

"""Session 管理的 9 个 HTTP 端点。

所有端点通过 app.state.server.runtime 访问 WingRuntime service 层。
subscribe/unsubscribe 需要 X-Client-Id header。
"""

from __future__ import annotations

import uuid

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request

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

router = APIRouter(tags=["session"])


# ============================================================
# 依赖注入
# ============================================================


def _require_client_id(x_client_id: str | None = Header(None)) -> str:
    """从 X-Client-Id header 提取 client_id，缺失时返回 400。"""
    if x_client_id is None:
        raise HTTPException(status_code=400, detail="missing X-Client-Id header")
    return x_client_id


def _get_server(request: Request):
    """从 app.state 获取 GatewayServer 实例。"""
    return request.app.state.server


# ============================================================
# Session 生命周期
# ============================================================


@router.post("/api/session/create", response_model=CreateSessionResponse)
async def create_session(
    body: CreateSessionRequest,
    request: Request,
) -> CreateSessionResponse:
    """创建新 session。"""
    server = _get_server(request)
    try:
        session = server.runtime.create_session(
            template_name=body.template_name,
            workspace=body.workspace,
        )
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return CreateSessionResponse(
        session_id=session.session_id,
        template_name=session.template_name,
        workspace=session.session_workspace,
    )


@router.post("/api/session/resume", response_model=ResumeSessionResponse)
async def resume_session(
    body: ResumeSessionRequest,
    request: Request,
) -> ResumeSessionResponse:
    """从磁盘恢复已有 session。"""
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


@router.post("/api/session/fork", response_model=ForkSessionResponse)
async def fork_session(
    body: ForkSessionRequest,
    request: Request,
) -> ForkSessionResponse:
    """从指定 session 的指定消息处分叉出新 session。"""
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


@router.post("/api/session/subscribe", response_model=OkResponse)
async def subscribe(
    body: SubscribeRequest,
    request: Request,
    x_client_id: str = Depends(_require_client_id),
) -> OkResponse:
    """订阅 session 事件。"""
    server = _get_server(request)

    # 验证 client 已连接
    if x_client_id not in server.clients:
        raise HTTPException(status_code=400, detail="client not connected")

    try:
        server.runtime.subscribe(x_client_id, body.session_id)
    except ValueError:
        raise HTTPException(status_code=404, detail="session not found")
    return OkResponse()


@router.post("/api/session/unsubscribe", response_model=OkResponse)
async def unsubscribe(
    body: UnsubscribeRequest,
    request: Request,
    x_client_id: str = Depends(_require_client_id),
) -> OkResponse:
    """取消订阅 session 事件。"""
    server = _get_server(request)
    server.runtime.unsubscribe(x_client_id, body.session_id)
    return OkResponse()


# ============================================================
# 消息发送
# ============================================================


@router.post("/api/session/send", response_model=SendMessageResponse)
async def send_message(
    body: SendMessageRequest,
    request: Request,
) -> SendMessageResponse:
    """通过 HTTP 向活跃 session 发送消息。"""
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


@router.get("/api/session/list", response_model=SessionListResponse)
async def list_sessions(request: Request) -> SessionListResponse:
    """列出所有 session。"""
    server = _get_server(request)
    sessions = server.runtime.list_sessions()
    return SessionListResponse(sessions=sessions)


@router.get("/api/session/get", response_model=SessionGetResponse)
async def get_session(
    request: Request,
    session_id: str = Query(...),
) -> SessionGetResponse:
    """获取指定 session 的完整状态。"""
    server = _get_server(request)
    state = server.runtime.get_session_state(session_id)
    if state is None:
        raise HTTPException(status_code=404, detail="session not found")
    return SessionGetResponse(**state)
