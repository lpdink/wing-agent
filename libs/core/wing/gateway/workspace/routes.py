# wing/gateway/workspace/routes.py — Workspace 文件浏览端点

"""``/api/workspace/*``——只读的文件浏览 HTTP 端点。

Route handler 只做：参数校验 → 调 ``service`` → 投影协议模型 / 组装 ``FileResponse``。
错误映射：``WorkspaceError`` → ``HTTPException``，由 app 级 exception handler 统一
输出 ``ErrorResponse`` 形状（error / detail 与其它端点一致）。

鉴权 / 限幅 / 符号链接等语义见 ``service.py`` 与 ``docs/dev/http-api.md``。
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException, Query
from starlette.responses import FileResponse

from . import service
from .protocol import (
    WorkspaceEntry,
    WorkspaceListResponse,
    WorkspaceRoot,
    WorkspaceRootsResponse,
)

router = APIRouter(tags=["workspace"])

#: 浏览器会**执行**的内容类型（文档上下文里能跑脚本 / 读同源能力）：直出时必须加
#: ``CSP: sandbox``。没有它时，直接导航 / iframe 预览 ``evil.html`` 会在网关 origin
#: （推荐部署下 = 前端 origin）执行文件里的脚本，把「任意文件读」升级成同源 XSS。
EXECUTABLE_MEDIA_TYPES = frozenset(
    {
        "text/html",
        "application/xhtml+xml",
        "image/svg+xml",
        "application/xml",
        "text/xml",
    }
)


def _read_headers(mime: str | None) -> dict[str, str]:
    """``read`` 的安全响应头。

    ``nosniff`` 恒在（禁止按内容嗅探线类型）；可执行文档类型加 ``CSP: sandbox``——
    沙箱把资源降成**唯一源**：禁脚本、禁同源读取，但渲染（HTML / SVG 预览）照常，
    即 GitHub raw 的标准做法。
    """
    headers = {"X-Content-Type-Options": "nosniff"}
    if mime in EXECUTABLE_MEDIA_TYPES:
        headers["Content-Security-Policy"] = "sandbox"
    return headers


def _project(entry: service.Entry) -> WorkspaceEntry:
    return WorkspaceEntry(
        name=entry.name,
        path=entry.path,
        type=entry.type,
        size=entry.size,
        mtime=entry.mtime,
        mime=entry.mime,
        symlink_target=entry.symlink_target,
        broken=entry.broken,
    )


def _http_error(exc: service.WorkspaceError) -> HTTPException:
    return HTTPException(status_code=exc.status, detail=exc.detail)


@router.get(
    "/api/workspace/roots",
    response_model=WorkspaceRootsResponse,
    summary="可浏览的起始目录",
)
def workspace_roots() -> WorkspaceRootsResponse:
    """可浏览的起点（home / 网关进程 cwd）——前端目录树与工作区选择的入口。"""
    return WorkspaceRootsResponse(
        roots=[
            WorkspaceRoot(path=root.path, kind=root.kind, name=root.name)
            for root in service.roots()
        ]
    )


@router.get(
    "/api/workspace/list",
    response_model=WorkspaceListResponse,
    summary="列举目录（分页）",
)
def workspace_list(
    path: str = Query(description="目录绝对路径"),
    offset: int = Query(default=0, ge=0, description="分页偏移（0 = 第一个条目）"),
    limit: int = Query(
        default=service.DEFAULT_LIST_LIMIT,
        ge=1,
        le=service.MAX_LIST_LIMIT,
        description=f"单页条数（1..{service.MAX_LIST_LIMIT}）",
    ),
) -> WorkspaceListResponse:
    """列举目录条目：目录在前、组内按名排序（全序，翻页稳定）。"""
    try:
        listing = service.list_dir(path, offset=offset, limit=limit)
    except service.WorkspaceError as exc:
        raise _http_error(exc) from exc
    return WorkspaceListResponse(
        path=listing.path,
        entries=[_project(entry) for entry in listing.entries],
        total=listing.total,
        next_offset=listing.next_offset,
    )


@router.get(
    "/api/workspace/stat",
    response_model=WorkspaceEntry,
    summary="条目元数据",
)
def workspace_stat(path: str = Query(description="条目绝对路径")) -> WorkspaceEntry:
    """单个条目的元数据（lstat 口径：符号链接不解析目标）。"""
    try:
        entry = service.stat_path(path)
    except service.WorkspaceError as exc:
        raise _http_error(exc) from exc
    return _project(entry)


@router.api_route(
    "/api/workspace/read",
    methods=["GET", "HEAD"],
    summary="读取文件（流式，支持 Range）",
    response_class=FileResponse,
)
def workspace_read(path: str = Query(description="文件绝对路径")) -> FileResponse:
    """读取常规文件：流式发送、支持 ``Range``（206 / 416 由 FileResponse 处理）。

    ``Content-Disposition: inline`` 带文件名；MIME 按扩展名猜（猜不出 =
    ``application/octet-stream``）。``HEAD`` 只回头（大小 / 类型 / Range 支持）。
    """
    try:
        target = service.resolve_read(path)
    except service.WorkspaceError as exc:
        raise _http_error(exc) from exc
    return FileResponse(
        target.path,
        media_type=target.mime,
        filename=target.name,
        content_disposition_type="inline",
        headers=_read_headers(target.mime),
        stat_result=target.stat,
    )
