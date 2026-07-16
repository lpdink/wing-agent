# wing_gateway/routes/session.py — Session HTTP 端点

"""Session 管理的 HTTP 端点。

所有端点通过 app.state.server.runtime 访问 WingRuntime service 层。
Route handler 只做参数验证和 HTTP 响应构造——业务逻辑在 WingRuntime 中。
subscribe/unsubscribe 需要 X-Client-Id header。
"""

from __future__ import annotations

import uuid

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request

from typing import TYPE_CHECKING

from wing.gateway.protocol import (
    BranchesResponse,
    CompactRequest,
    CompactResponse,
    ContextStatsInfo,
    CreateSessionRequest,
    CreateSessionResponse,
    ForkSessionRequest,
    ForkSessionResponse,
    InterruptRequest,
    OkResponse,
    ResumeSessionRequest,
    ResumeSessionResponse,
    RewindRequest,
    RewindResponse,
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
from wing.event.query_response import BranchTargetInfo

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
    """创建新 session。"""
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


@router.post(
    "/api/session/fork",
    response_model=ForkSessionResponse,
    summary="分叉 session",
)
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
    """订阅 session 事件。需要 X-Client-Id header。"""
    server = _get_server(request)
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
    """取消订阅 session 事件。"""
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
    """通过 HTTP 向活跃 session 发送消息。"""
    server = _get_server(request)
    request_id = uuid.uuid4().hex

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
    """获取指定 session 的完整状态。"""
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
    """获取 session 的运行时状态：模型、工具、token 用量等。"""
    server = _get_server(request)
    session = server.runtime.get_session(session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    status = session.agent.get_status()
    cm = session.agent.context_manager
    msg_count, total_tok = cm.get_context_stats()
    return SessionInfoResponse(
        model=status["model"],
        api_url=status["api_url"],
        tools=status["tools"],
        total_tokens=status["total_tokens"],
        context_window_tokens=status["context_window_tokens"],
        thinking=status["thinking"],
        reasoning_effort=status["reasoning_effort"],
        yolo=session.agent.yolo,
        session_name=session.session_name,
        context_stats=ContextStatsInfo(
            message_count=msg_count,
            total_tokens=total_tok,
        ),
        skills_info=cm.get_skills_info(),
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
    """获取 session 的可回退/分叉消息节点列表。"""
    server = _get_server(request)
    session = server.runtime.get_session(session_id)
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
    """统一的 session 状态变更端点。"""
    server = _get_server(request)

    if all(
        v is None
        for v in (
            body.model,
            body.agent,
            body.title,
            body.thinking,
            body.reasoning_effort,
            body.yolo,
        )
    ):
        raise HTTPException(
            status_code=400, detail="at least one update field is required"
        )

    try:
        await server.runtime.update_session(
            session_id=body.session_id,
            model=body.model,
            agent=body.agent,
            title=body.title,
            thinking=body.thinking,
            reasoning_effort=body.reasoning_effort,
            yolo=body.yolo,
        )
    except ValueError as e:
        err = str(e)
        if "not found" in err.lower():
            raise HTTPException(status_code=404, detail=err)
        raise HTTPException(status_code=400, detail=err)

    return UpdateSessionResponse(ok=True)


# ============================================================
# Session 操作端点（原魔术命令迁移）
# ============================================================


@router.post(
    "/api/session/compact",
    response_model=CompactResponse,
    summary="压缩 session 上下文",
)
async def compact_session(
    body: CompactRequest,
    request: Request,
) -> CompactResponse:
    """压缩指定 session 的上下文。"""
    server = _get_server(request)
    try:
        original, compressed = await server.runtime.compact_session(body.session_id)
    except ValueError:
        raise HTTPException(status_code=404, detail="session not found")
    except RuntimeError as e:
        raise HTTPException(status_code=400, detail=str(e))
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"compact failed: {e}")

    return CompactResponse(
        ok=True, original_tokens=original, compressed_tokens=compressed
    )


@router.post(
    "/api/session/interrupt",
    response_model=OkResponse,
    summary="中断 session 当前任务",
)
async def interrupt_session(
    body: InterruptRequest,
    request: Request,
) -> OkResponse:
    """中断指定 session 的当前 agent 任务。"""
    server = _get_server(request)
    try:
        server.runtime.interrupt_session(body.session_id)
    except ValueError:
        raise HTTPException(status_code=404, detail="session not found")
    return OkResponse()


@router.post(
    "/api/session/rewind",
    response_model=RewindResponse,
    summary="回退 session 到指定消息",
)
async def rewind_session(
    body: RewindRequest,
    request: Request,
) -> RewindResponse:
    """回退指定 session 到 target_uuid 处的消息。"""
    server = _get_server(request)
    try:
        draft = server.runtime.rewind_session(body.session_id, body.target_uuid)
    except ValueError as e:
        err = str(e)
        if "not found" in err.lower():
            raise HTTPException(status_code=404, detail=err)
        raise HTTPException(status_code=400, detail=err)

    return RewindResponse(ok=True, draft=draft)
