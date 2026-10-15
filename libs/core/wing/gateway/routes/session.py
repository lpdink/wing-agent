# wing/gateway/routes/session.py — Session HTTP 端点

"""Session 管理的 HTTP 端点。

Route handler 只做：参数验证 → 调 Runtime → 构造 HTTP 响应。
异常映射：LookupError → 404, ValueError → 400, RuntimeError → 400/500。
归档例外：SessionArchivedError **不是** LookupError（可行动的存在性结论），由
app 级 handler 单独映射 404 + error="session_archived"，提示"先恢复再使用"。
"""

from __future__ import annotations

import asyncio
import uuid

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request

from typing import TYPE_CHECKING, get_args

from wing.gateway.protocol import (
    ArchiveRequest,
    ArchiveResponse,
    ArchivedSessionsResponse,
    BranchesResponse,
    CompactRequest,
    CompactResponse,
    CreateSessionRequest,
    CreateSessionResponse,
    ForkSessionRequest,
    ForkSessionResponse,
    InterruptRequest,
    MediaUploadResponse,
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
from wing.common.utils import InvalidInputError
from wing.common.logger import log
from wing.config import get_config
from wing.event import SessionStatus
from wing.media import too_large_message
from wing.request_context import session_context
from wing.session.tags import validate_tag

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
            media=body.media,
        )
    except InvalidInputError as e:
        # 正文非法（不可编码为 UTF-8）/ 媒体引用不可兑现（幽灵引用 / 非法 id）：
        # 输入问题 → 400，不是 500。
        # 只认这一种——链上将来出现的其它 ValueError 是内部故障，不该被报成
        # "客户端的错"（500 让它带着栈进日志，而不是骗调用方去改请求）。
        raise HTTPException(status_code=400, detail=str(e))
    return SendMessageResponse(ok=True, request_id=request_id)


# ============================================================
# 媒体上传
# ============================================================


def _declared_content_length(request: Request) -> int | None:
    """``Content-Length`` 头解析成整数（缺失 / 不可解析 = ``None``）。

    不可解析的值不在这里拒绝：声明值与真实字节不符（缺失 / 说谎的头）一律由字节闸门
    按**真实字节**兜底。
    """
    raw = request.headers.get("content-length")
    if raw is None:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


@router.post(
    "/api/session/media",
    response_model=MediaUploadResponse,
    summary="上传媒体（原始字节）",
)
async def upload_session_media(
    request: Request,
    session_id: str = Query(..., description="目标 session ID"),
    name: str | None = Query(
        None, description="媒体展示名（建议给 basename；缺省 / 空串 = 不记录）"
    ),
) -> MediaUploadResponse:
    """上传一份原始图片字节到会话媒体目录，返回可被消息引用的 ``media``。

    body 是**原始字节**（不是 JSON / 不是 multipart），大小上限 = ``images.max_bytes``，
    格式按 magic bytes 判定（png/jpeg/webp/gif）；id = 字节的 sha256，同字节重复上传返回
    同一 id（幂等）。会话必须先存在（本端点**不隐式建会话**，也不水合已逐出的会话）。

    校验（会话存在 → 字节非空 → 上限 → 格式 → 尺寸）全部发生在落盘之前：失败（400/404）
    不产生任何存储副作用。响应里的 ``media`` 可直接作为消息的 ``media`` 元素回投。

    **校验顺序 vs 收包顺序**：``request.body()`` 没有大小上限（Starlette 会先把整个
    body 缓冲进内存），因此这里刻意不先收包——顺序是「会话存在性（404）→
    ``Content-Length`` 预检（声明值超 ``images.max_bytes`` 直接 400，一个字节都不读）
    → 收包 → 字节闸门」。幽灵会话与声明超限的请求都不必先被吞进内存；声明值与真实
    字节不符（缺头 / 说谎的头）由闸门按真实字节兜底。
    """
    server = _get_server(request)

    # 会话存在性先于收包：404 不该以"读完整个 body"为代价（与 send 同序：存在性先于
    # 内容校验，body 合法或非法都回 404）。
    if not server.runtime.session_exists(session_id):
        raise HTTPException(status_code=404, detail="session not found")

    # Content-Length 预检（收包之前）：超限的声明值不必先读进内存再在闸门里拒绝。
    max_bytes = get_config().images.max_bytes
    declared = _declared_content_length(request)
    if declared is not None and declared > max_bytes:
        raise HTTPException(
            status_code=400, detail=too_large_message(declared, max_bytes)
        )

    data = await request.body()
    try:
        media = server.runtime.upload_media(session_id, data, name=name)
    except LookupError:
        # 预检之后的解析只会在这里再失败于竞态（会话在两步之间消失）——保留映射。
        raise HTTPException(status_code=404, detail="session not found")
    except InvalidInputError as e:
        # 字节不合规（空 / 超限 / 非支持格式 / 尺寸不可解析）——输入问题 → 400。
        # 与 send 同一条窄捕获：写失败（OSError 等）是服务端故障，不在此拦。
        raise HTTPException(status_code=400, detail=str(e))
    return MediaUploadResponse(ok=True, media=media)


# 查询过滤参数解析（列表端点的子集过滤）
# ============================================================

#: 状态过滤的取值域 = 会话状态全集（唯一声明处 `wing.event.SessionStatus`）。
_STATUS_FILTER_VALUES = frozenset(get_args(SessionStatus))

#: 取值域的展示形态（OpenAPI 描述与 400 报错共用，避免复制字面量）。
_STATUS_FILTER_VALUES_TEXT = ", ".join(sorted(_STATUS_FILTER_VALUES))


def _parse_status_filter(raw: str | None) -> set[str] | None:
    """`?status=idle,working` → 状态集合；缺省（None）= 不过滤。

    空段 / 未知取值一律 ValueError（路由折成 400）：过滤是子集语义，静默丢弃
    一个拼错的取值会让调用方以为过滤生效了。
    """
    if raw is None:
        return None
    values = [part.strip() for part in raw.split(",")]
    if any(not value for value in values):
        raise ValueError("status filter must not contain empty values")
    unknown = sorted(set(values) - _STATUS_FILTER_VALUES)
    if unknown:
        raise ValueError(
            f"unknown status filter value(s): {', '.join(unknown)}; "
            f"available: {_STATUS_FILTER_VALUES_TEXT}"
        )
    return set(values)


def _reject_repeated_filters(request: Request) -> None:
    """同一过滤参数重复给出 → 400。

    重复参数会被 FastAPI 静默 last-wins（`?status=bogus&status=idle` 只剩后者、
    `?tag=pin&tag=crew` 只按 crew 过滤）——与"非法取值一律 400"的立场冲突。
    多值用单个逗号分隔参数表达（`?status=idle,working`）。
    """
    for name in ("status", "tag"):
        if len(request.query_params.getlist(name)) > 1:
            raise ValueError(
                f"{name} filter must be given once (pass a single comma-separated value)"
            )


def _parse_tag_filter(raw: str | None) -> list[str]:
    """`?tag=pin,crew` → 标签列表（AND：必须全部携带）；缺省 = 不过滤。

    值走写入侧同一套校验（:func:`wing.session.tags.validate_tag` 是标签词汇的
    唯一入口）：**词法非法**（空 / 超长 / 空白 / 控制符 / `-` 开头）一律 400
    ——这类值不可能命中任何行（列表行的 tags 恒为清洗后的合法值），静默返回空
    结果只会掩盖调用方的笔误；**语法合法但不存在**的标签则是空结果（词表开放，
    无从枚举）。
    """
    if raw is None:
        return []
    tags: list[str] = []
    for part in raw.split(","):
        try:
            tags.append(validate_tag(part.strip()))
        except ValueError as e:
            raise ValueError(f"tag filter: {e}")
    return tags


# ============================================================
# 查询
# ============================================================


@router.get(
    "/api/session/list",
    response_model=SessionListResponse,
    summary="列出所有 session（可按状态 / 标签过滤）",
)
async def list_sessions(
    request: Request,
    status: str | None = Query(
        None,
        description=(
            f"按会话状态过滤（逗号分隔：{_STATUS_FILTER_VALUES_TEXT}）；"
            "只请求在内存的会话即 `status=idle,working,waiting`"
        ),
    ),
    tag: str | None = Query(
        None,
        description="按标签过滤（逗号分隔，AND：必须全部携带）",
    ),
) -> SessionListResponse:
    """列表 / 子集过滤共用一条查询。

    过滤是**子集**语义：结果是全量列表的子序列（同样的行、同样的相对顺序）。
    非法过滤值一律 400（静默忽略会让调用方以为过滤生效了）。
    """
    server = _get_server(request)
    try:
        _reject_repeated_filters(request)
        statuses = _parse_status_filter(status)
        tags = _parse_tag_filter(tag)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    sessions = server.runtime.list_sessions(statuses=statuses, tags=tags)
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
            body.model_id,
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

    try:
        await server.runtime.update_session(
            session_id=body.session_id,
            model_id=body.model_id,
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
    # 压缩是会话级操作：绑定归属，压缩链路的日志（含下面的失败日志）自动
    # 带上 session；缺失该绑定时这些行只能靠 message 文本对号入座。
    with session_context(body.session_id):
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


# ============================================================
# 归档（archive）
# ============================================================


@router.post(
    "/api/session/archive",
    response_model=ArchiveResponse,
    summary="归档 / 恢复 session（设置归档状态）",
)
async def archive_session(
    body: ArchiveRequest,
    request: Request,
) -> ArchiveResponse:
    """把一个会话移入 / 移出归档区（幂等）。

    归档区（``<sessions root>/.archive/``）不参与列表与解析：归档后的会话从
    ``/api/session/list`` 消失，resume / send / tag 等一律 404 +
    ``error="session_archived"``（提示先恢复）。移动是纯 rename——不改任何字节。

    钉住条件与 release 同一套：忙碌 / 有待处理输入 / 被订阅 / 非归档后端一律
    409 拒绝；空闲可收回的会话会先逐出再移动。幂等：归档已归档的会话 /
    恢复未归档的会话返回 200 + ``changed=false``。
    """
    server = _get_server(request)
    try:
        if body.archived:
            changed, detail = server.runtime.archive_session(body.session_id)
        else:
            changed, detail = server.runtime.unarchive_session(body.session_id)
    except LookupError:
        raise HTTPException(status_code=404, detail="session not found")
    except RuntimeError as e:
        raise HTTPException(status_code=409, detail=str(e))
    return ArchiveResponse(
        ok=True,
        session_id=body.session_id,
        archived=body.archived,
        changed=changed,
        detail=detail,
    )


@router.get(
    "/api/session/archive",
    response_model=ArchivedSessionsResponse,
    summary="列出归档区 session（读索引，不扫会话目录）",
)
async def list_archived_sessions(request: Request) -> ArchivedSessionsResponse:
    """归档区条目（标题 / tags / 模型 / 归档时间）。

    读取归档索引（``.archive/index.json``）——归档是低频路径，列表不该为此
    扫全量会话目录；索引缺失 / 损坏时按归档目录重建。模型展示名按当前配置
    现算（与 ``/api/session/list`` 同一口径）。
    """
    server = _get_server(request)
    return ArchivedSessionsResponse(sessions=server.runtime.list_archived_sessions())
