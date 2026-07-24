# wing_gateway/routes/session.py — Session HTTP 端点

"""Session 管理的 HTTP 端点。

Route handler 只做：参数验证 → 调 Runtime → 构造 HTTP 响应。
异常映射：LookupError → 404, ValueError → 400, RuntimeError → 400/500。
"""

from __future__ import annotations

import asyncio
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
from wing.common.logger import log
from wing.event.query_response import BranchTargetInfo

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

router = APIRouter(tags=["session"])


# ============================================================
# 依赖注入
# ============================================================


def _require_client_id(x_client_id: str | None = Header(None)) -> str:
    if x_client_id is None:
        raise HTTPException(status_code=400, detail="missing X-Client-Id header")
    return x_client_id


def _get_server(request: Request) -> GatewayServer:
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
    server = _get_server(request)
    try:
        session = server.runtime.resume_session(body.session_id)
    except LookupError:
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
    server = _get_server(request)
    try:
        new_session, draft = server.runtime.fork_session(
            source_session_id=body.source_session_id,
            target_uuid=body.target_uuid,
        )
    except LookupError:
        raise HTTPException(status_code=404, detail="session or uuid not found")
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
    server = _get_server(request)
    if x_client_id not in server.clients:
        raise HTTPException(status_code=400, detail="client not connected")
    try:
        server.runtime.subscribe(x_client_id, body.session_id)
    except LookupError:
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
    server = _get_server(request)
    request_id = uuid.uuid4().hex

    if server.runtime.get_session_state(body.session_id) is None:
        raise HTTPException(status_code=404, detail="session not found")

    await server.runtime.post(
        content=body.content,
        request_id=request_id,
        session_id=body.session_id,
        tool_call_id=body.tool_call_id,
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
        workdir=session.session_workspace,
        status=session.status,
        context_stats=ContextStatsInfo(
            message_count=msg_count,
            total_tokens=total_tok,
        ),
        skills_info=cm.get_skills_info(),
        system_prompt=cm.system_prompt.content or "",
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
            body.workspace,
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
            workspace=body.workspace,
        )
    except LookupError as e:
        raise HTTPException(status_code=404, detail=str(e))
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))

    return UpdateSessionResponse(ok=True)


# ============================================================
# Session 操作端点
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
    server = _get_server(request)
    try:
        original, compressed = await asyncio.wait_for(
            server.runtime.compact_session(body.session_id), timeout=60.0
        )
    except asyncio.TimeoutError:
        raise HTTPException(status_code=504, detail="compact timed out (60s)")
    except LookupError:
        raise HTTPException(status_code=404, detail="session not found")
    except RuntimeError as e:
        raise HTTPException(status_code=400, detail=str(e))
    except Exception as e:
        log.error(f"compact failed: {e}")
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
    server = _get_server(request)
    try:
        server.runtime.interrupt_session(body.session_id)
    except LookupError:
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
    server = _get_server(request)
    try:
        draft = server.runtime.rewind_session(body.session_id, body.target_uuid)
    except LookupError:
        raise HTTPException(status_code=404, detail="session not found")
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))

    return RewindResponse(ok=True, draft=draft)
