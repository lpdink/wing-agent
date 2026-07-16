# wing_gateway/routes/session.py — Session HTTP 端点

"""Session 管理的 9 个 HTTP 端点。

所有端点通过 app.state.server.runtime 访问 WingRuntime service 层。
subscribe/unsubscribe 需要 X-Client-Id header。
"""

from __future__ import annotations

import uuid

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request

from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.event import (
    BranchTargetInfo,
    BranchTargetsEvent,
    CompactDoneEvent,
    ContextStatsEvent,
    EventTarget,
    InterruptedEvent,
    SessionStateChangedEvent,
    SyncSessionEvent,
)
from wing.event_bus import event_bus
from wing.schema import Message
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
from wing.magic_command.registry import magic_registry

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer
    from wing.session import Session

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

    支持模型切换、agent 模板切换、标题设置、thinking/yolo 模式开关及推理力度设置。
    多字段同时更新时按 agent → model → title → thinking → reasoning_effort → yolo 顺序执行。
    至少提供一个非 None 字段，否则返回 400。session 不存在时返回 404。
    """
    server = _get_server(request)

    # 校验至少提供一个更新字段
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

    session = server.runtime.sm.get_session(body.session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    # 按 agent → model → title → thinking → reasoning_effort → yolo 顺序执行
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

    if body.reasoning_effort is not None:
        session.agent.set_reasoning_effort(body.reasoning_effort)

    if body.yolo is not None:
        session.agent.set_yolo(body.yolo)

    # emit SessionStateChangedEvent 通知前端状态已变更
    # agent 切换会重置 thinking/reasoning_effort/yolo，需同步报告新值
    agent_switched = body.agent is not None
    event_bus.emit(
        SessionStateChangedEvent(
            session_id=session.session_id,
            model=session.agent.model
            if body.model is not None or agent_switched
            else None,
            thinking=session.agent.model_provider.thinking
            if body.thinking is not None or agent_switched
            else None,
            reasoning_effort=session.agent.model_provider.reasoning_effort
            if body.reasoning_effort is not None or agent_switched
            else None,
            yolo=session.agent.yolo
            if body.yolo is not None or agent_switched
            else None,
            title=session.session_name if body.title is not None else None,
            agent=session.template_name if agent_switched else None,
        )
    )

    return UpdateSessionResponse(ok=True)


# ============================================================
# Session 操作端点（原魔术命令迁移）
# ============================================================


def _emit_context_stats(session: "Session") -> None:
    """emit ContextStatsEvent — compact 和 rewind 改变上下文后共用。"""
    cm = session.agent.context_manager
    count, tokens = cm.get_context_stats()
    ctx_window = 0
    if cm.compactor:
        ctx_window = cm.compactor.context_window_tokens
    event_bus.emit(
        ContextStatsEvent(
            session_id=session.session_id,
            message_count=count,
            total_tokens=tokens,
            context_window_tokens=ctx_window,
        )
    )


@router.post(
    "/api/session/compact",
    response_model=CompactResponse,
    summary="压缩 session 上下文",
)
async def compact_session(
    body: CompactRequest,
    request: Request,
) -> CompactResponse:
    """压缩指定 session 的上下文。

    调用 compactor 对消息链进行摘要压缩，emit CompactDoneEvent 和
    ContextStatsEvent 通知订阅客户端。session 不存在返回 404，
    未配置 compactor 返回 400。
    """
    server = _get_server(request)
    session = server.runtime.sm.get_session(body.session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    agent = session.agent
    cm = agent.context_manager
    if not cm.compactor:
        raise HTTPException(status_code=400, detail="compactor not configured")

    # 丢弃任何 pending async compact
    cm._discard_pending_compact()
    msgs = list(cm._messages)
    try:
        full_context = [cm.system_prompt] + msgs
        compacted = await cm.compactor.do_compact(
            full_context,
            agent.model,
            agent.model_provider,
            tools=agent.tools,
        )
        last_compressed_uuid = msgs[-1].uuid if msgs else None
        compact_node = Message(
            role="assistant",
            content=compacted.content,
            parent_uuid=None,
            unzip_last_uuid=last_compressed_uuid,
        )
        compact_node.uuid = __import__("uuid").uuid4().hex

        cm._messages.append_detached(compact_node)
        cm._messages.set_tip(compact_node.uuid)

        event_bus.emit(
            CompactDoneEvent(
                session_id=session.session_id,
                original_tokens=compacted.usage.prompt_tokens,
                compressed_tokens=compacted.usage.completion_tokens,
                model=agent.model,
            )
        )
        _emit_context_stats(session)

        return CompactResponse(
            ok=True,
            original_tokens=compacted.usage.prompt_tokens,
            compressed_tokens=compacted.usage.completion_tokens,
        )
    except Exception as e:
        log.error(f"compact failed: {e}")
        raise HTTPException(status_code=500, detail="compact failed")


@router.post(
    "/api/session/interrupt",
    response_model=OkResponse,
    summary="中断 session 当前任务",
)
async def interrupt_session(
    body: InterruptRequest,
    request: Request,
) -> OkResponse:
    """中断指定 session 的当前 agent 任务。

    清空 agent inbox，emit InterruptedEvent 通知订阅客户端。
    session 不存在返回 404。
    """
    server = _get_server(request)
    session = server.runtime.sm.get_session(body.session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    session.agent.interrupt()
    event_bus.emit(InterruptedEvent(session_id=session.session_id))
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
    """回退指定 session 到 target_uuid 处的消息。

    emit SyncSessionEvent（含 draft）和 BranchTargetsEvent 通知订阅客户端。
    session 不存在返回 404，target_uuid 无效返回 400。
    """
    server = _get_server(request)
    session = server.runtime.sm.get_session(body.session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    cm = session.agent.context_manager
    try:
        draft = cm.rewind(body.target_uuid)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))

    _emit_context_stats(session)

    # 通知客户端同步 session 状态
    event_bus.emit(
        SyncSessionEvent(
            session_id=session.session_id,
            messages=[msg.model_dump() for msg in cm.get_context_window()],
            agent=None,
            draft=draft,
        )
    )

    # 刷新 branch targets 缓存
    targets = cm.get_branch_targets()
    event_bus.emit(
        BranchTargetsEvent(
            session_id=session.session_id,
            targets=[BranchTargetInfo(**t) for t in targets],
        )
    )

    return RewindResponse(ok=True, draft=draft)
