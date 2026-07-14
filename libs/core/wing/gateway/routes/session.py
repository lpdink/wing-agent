# wing_gateway/routes/session.py — Session HTTP 端点

"""Session 管理的 9 个 HTTP 端点。

所有端点通过 app.state.server.runtime 访问 WingRuntime service 层。
subscribe/unsubscribe 需要 X-Client-Id header。
"""

from __future__ import annotations

import uuid

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request

from typing import TYPE_CHECKING

from wing.event import SessionStateChangedEvent
from wing.event.query_response import BranchTargetInfo
from wing.event_bus import event_bus
from wing.gateway.protocol import (
    BranchesResponse,
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
    SessionInfoResponse,
    SessionListResponse,
    SubscribeRequest,
    UnsubscribeRequest,
    UpdateSessionRequest,
    UpdateSessionResponse,
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


# ============================================================
# Session 运行时查询
# ============================================================


@router.get(
    "/api/session/info",
    response_model=SessionInfoResponse,
    summary="获取 session 运行时状态",
)
async def session_info(
    request: Request,
    session_id: str = Query(..., description="目标 session ID"),
) -> SessionInfoResponse:
    """获取 session 的运行时状态：模型、工具、token 用量等。

    session 不存在时返回 404。
    """
    server = _get_server(request)
    session = server.runtime.sm.get_session(session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    status = session.agent.get_status()
    return SessionInfoResponse(
        model=status["model"],
        api_url=status["api_url"],
        tools=status["tools"],
        total_tokens=status["total_tokens"],
        context_window_tokens=status["context_window_tokens"],
        thinking=status["thinking"],
        yolo=session.agent.yolo,
        session_name=session.session_name,
    )


@router.get(
    "/api/session/branches",
    response_model=BranchesResponse,
    summary="获取可回退/分叉的消息节点",
)
async def session_branches(
    request: Request,
    session_id: str = Query(..., description="目标 session ID"),
) -> BranchesResponse:
    """获取 session 的可回退/分叉消息节点列表。

    用于 /rewind 和 /fork 命令的候选面板。session 不存在时返回 404。
    """
    server = _get_server(request)
    session = server.runtime.sm.get_session(session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    raw_targets = session.agent.context_manager.get_branch_targets()
    targets = [BranchTargetInfo(**t) for t in raw_targets]
    return BranchesResponse(targets=targets)


# ============================================================
# Session 状态变更
# ============================================================


@router.post(
    "/api/session/update",
    response_model=UpdateSessionResponse,
    summary="更新 session 状态",
)
async def update_session(
    body: UpdateSessionRequest,
    request: Request,
) -> UpdateSessionResponse:
    """统一的 session 状态变更端点。

    支持模型切换、agent 模板切换、标题设置、thinking/yolo 模式开关。
    多字段同时更新时按 agent → model → title → thinking → yolo 顺序执行。
    至少提供一个非 None 字段，否则返回 400。session 不存在时返回 404。
    """
    server = _get_server(request)

    # 校验至少提供一个更新字段
    if all(
        v is None
        for v in (body.model, body.agent, body.title, body.thinking, body.yolo)
    ):
        raise HTTPException(
            status_code=400, detail="at least one update field is required"
        )

    session = server.runtime.sm.get_session(body.session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    # 按 agent → model → title → thinking → yolo 顺序执行
    if body.agent is not None:
        template = server.runtime.template_manager.get(body.agent)
        if template is None:
            available = server.runtime.template_manager.all_names
            raise HTTPException(
                status_code=400,
                detail=f"template '{body.agent}' not found, available: {available}",
            )
        await session.switch_template(template)

    if body.model is not None:
        session.agent.model = body.model

    if body.title is not None:
        session.set_title(body.title)

    if body.thinking is not None:
        session.agent.model_provider.set_thinking(body.thinking)

    if body.yolo is not None:
        session.agent.set_yolo(body.yolo)

    # emit SessionStateChangedEvent 通知前端状态已变更
    event_bus.emit(
        SessionStateChangedEvent(
            session_id=session.session_id,
            model=session.agent.model
            if body.model is not None or body.agent is not None
            else None,
            thinking=session.agent.model_provider.thinking
            if body.thinking is not None
            else None,
            yolo=session.agent.yolo if body.yolo is not None else None,
            title=session.session_name if body.title is not None else None,
            agent=session.template_name if body.agent is not None else None,
        )
    )

    return UpdateSessionResponse(ok=True)
