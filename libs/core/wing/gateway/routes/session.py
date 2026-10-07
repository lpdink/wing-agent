# wing/gateway/routes/session.py — Session HTTP 端点

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
    CreateSessionRequest,
    CreateSessionResponse,
    ForkSessionRequest,
    ForkSessionResponse,
    InterruptRequest,
    OkResponse,
    ReleaseRequest,
    ReleaseResponse,
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
    TagSessionRequest,
    TagSessionResponse,
    UnsubscribeRequest,
    UpdateSessionRequest,
    UpdateSessionResponse,
)
from wing.gateway.projection import build_session_branches, build_session_info
from wing.common.logger import log

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
            backend=body.backend,
            tags=body.tags,
            session_id=body.session_id,
        )
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return CreateSessionResponse(
        session_id=session.session_id,
        template_name=session.template_name or "",
        workspace=session.session_workspace,
        backend=session.store.name,
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
    """恢复既有会话；可选 `agent` 覆盖只应用 model/provider/effort/tools 子集。

    覆盖的参数校验不过（工具引用无法解析）→ 400；会话不存在 → 404。
    """
    server = _get_server(request)
    try:
        session = server.runtime.resume_session(
            body.session_id, agent_override=body.agent
        )
    except LookupError:
        raise HTTPException(status_code=404, detail="session not found")
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
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

    # 被逐出（不在内存）的会话按需水合；磁盘上也没有才 404。
    try:
        server.runtime.ensure_loaded(body.session_id)
    except LookupError:
        raise HTTPException(status_code=404, detail="session not found")

    try:
        await server.runtime.post(
            content=body.content,
            request_id=request_id,
            session_id=body.session_id,
            tool_call_id=body.tool_call_id,
        )
    except ValueError as e:
        # 正文非法（不可编码为 UTF-8）：输入问题 → 400，不是 500。
        raise HTTPException(status_code=400, detail=str(e))
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
    return build_session_info(session)


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
    return build_session_branches(session)


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
            body.provider,
            body.agent,
            body.title,
            body.thinking,
            body.reasoning_effort,
            body.yolo,
            body.workspace,
            body.tools,
        )
    ):
        raise HTTPException(
            status_code=400, detail="at least one update field is required"
        )

    if (body.model is None) != (body.provider is None):
        raise HTTPException(
            status_code=400,
            detail="model and provider must be set together or both omitted",
        )

    try:
        await server.runtime.update_session(
            session_id=body.session_id,
            model=body.model,
            provider=body.provider,
            agent=body.agent,
            title=body.title,
            thinking=body.thinking,
            reasoning_effort=body.reasoning_effort,
            yolo=body.yolo,
            workspace=body.workspace,
            tools=body.tools,
        )
    except LookupError as e:
        raise HTTPException(status_code=404, detail=str(e))
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))

    return UpdateSessionResponse(ok=True)


@router.post(
    "/api/session/tag",
    response_model=TagSessionResponse,
    summary="读取或增删 session 标签",
)
async def tag_session(
    body: TagSessionRequest,
    request: Request,
) -> TagSessionResponse:
    """标签读 / 写同一端点：``add`` / ``remove`` 皆缺省 = 纯读取。

    不水合已逐出会话（标签属于持久 metadata，读或写都不把会话换入内存）；
    增删在服务端一次原子应用（单进程内：内存态与磁盘态同源；跨进程共享
    同一 file store 时沿用既有 metadata 的"最后写者覆盖"语义）。
    """
    server = _get_server(request)
    try:
        mutation = server.runtime.set_session_tags(
            body.session_id,
            add=body.add or (),
            remove=body.remove or (),
        )
    except LookupError as e:
        raise HTTPException(status_code=404, detail=str(e))
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))

    return TagSessionResponse(
        ok=True,
        session_id=body.session_id,
        tags=mutation.tags,
        added=mutation.added,
        removed=mutation.removed,
        tag_meta=mutation.tag_meta,
    )


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
            server.runtime.compact_session(body.session_id, body.instruction),
            timeout=1200.0,
        )
    except asyncio.TimeoutError:
        raise HTTPException(status_code=504, detail="compact timed out (1200s)")
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
    """中断会话；agent 侧收口等待有界（取消阶梯），请求保证返回。"""
    server = _get_server(request)
    try:
        await server.runtime.interrupt_session(body.session_id)
    except LookupError:
        raise HTTPException(status_code=404, detail="session not found")
    return OkResponse()


@router.post(
    "/api/session/release",
    response_model=ReleaseResponse,
    summary="逐出 session 内存态（release）",
)
async def release_session(
    body: ReleaseRequest,
    request: Request,
) -> ReleaseResponse:
    """逐出（eviction）——只回收内存态，磁盘状态不动。

    忽略空闲时长（不为 TTL 等待），但不忽略钉住条件：忙碌 / 有待处理输入 /
    被订阅 / 非持久后端的会话一律 409 拒绝，附带原因。
    """
    server = _get_server(request)
    try:
        released, detail = server.runtime.release_session(body.session_id)
    except LookupError:
        raise HTTPException(status_code=404, detail="session not found")
    except RuntimeError as e:
        raise HTTPException(status_code=409, detail=str(e))
    return ReleaseResponse(ok=True, released=released, detail=detail)


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
