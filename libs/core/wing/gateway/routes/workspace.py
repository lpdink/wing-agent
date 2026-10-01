# wing/gateway/routes/workspace.py — 会话工作区文件端点

"""受限的工作区图片端点（接口冻结，web 客户端按此对接）。

    GET /api/workspace/image?session_id=<id>&path=<urlencoded>

浏览器没有 webview URI 这类宿主通路，"transcript 里的工作区本地图片"只能经网关拿：
本端点只服务**会话 workspace 内**、扩展名在白名单内、单文件 ≤ 10 MiB 的图片。

状态码语义（冻结）：

| 码 | 条件 |
|----|------|
| 200 | 图片字节 + 正确 Content-Type |
| 400 | 缺参 / 坏参（空串、全空白、NUL 字节） |
| 403 | 解析后越界（含符号链接逃逸；`..` 的 URL 编码形态同样落这里） |
| 404 | 会话不在内存 / 无 workspace / 扩展名不在白名单 / 文件不存在或非普通文件 |
| 413 | 文件大于 10 MiB |
| 401 | auth 开启且未带 key（由 AuthMiddleware 统一给出） |

不实现 Range：读全量字节后以普通 `Response` 返回（`FileResponse` 会自动支持 Range
并可能回 206，与本接口的冻结语义不符）。
"""

from __future__ import annotations

import stat

from fastapi import APIRouter, HTTPException, Query, Request
from starlette.concurrency import run_in_threadpool
from starlette.responses import Response

from wing.gateway.file_policy import (
    CACHE_CONTROL_IMAGE,
    IMAGE_MAX_BYTES,
    BadPathError,
    PathOutsideRootError,
    image_content_type,
    resolve_within_root,
)

router = APIRouter(tags=["workspace"])


def _required(value: str | None, field: str) -> str:
    """必填查询参数：缺失 / 空串 / 全空白一律 400（不用 FastAPI 的 422）。"""
    if value is None or not value.strip():
        raise HTTPException(status_code=400, detail=f"missing or empty '{field}'")
    return value


@router.get(
    "/api/workspace/image",
    summary="读取会话工作区内的图片",
    response_class=Response,
    responses={
        200: {"content": {"image/png": {}}, "description": "图片字节"},
        400: {"description": "缺参 / 坏参"},
        403: {"description": "路径越界（含符号链接逃逸）"},
        404: {"description": "会话 / 文件不存在，或扩展名不在白名单"},
        413: {"description": "超过 10 MiB 上限"},
    },
)
async def read_workspace_image(
    request: Request,
    session_id: str | None = Query(default=None, description="目标 session ID"),
    path: str | None = Query(
        default=None,
        description="图片路径：相对 session workspace，或落在其中的绝对路径",
    ),
) -> Response:
    """见模块文档的状态码表。"""
    session_id = _required(session_id, "session_id")
    raw_path = _required(path, "path")

    # 会话只认内存态（与 /api/session/info、/branches 同口径）：图片是渲染素材，
    # 不该把已逐出的会话重新水合进内存；web 只在订阅（= 按需水合）后重放 transcript。
    session = request.app.state.server.runtime.get_session(session_id)
    if session is None:
        raise HTTPException(status_code=404, detail="session not found")

    workspace = session.session_workspace
    if not workspace:
        raise HTTPException(status_code=404, detail="session workspace is not set")

    try:
        target = resolve_within_root(workspace, raw_path)
    except BadPathError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    except PathOutsideRootError:
        raise HTTPException(
            status_code=403, detail="path escapes the session workspace"
        )

    # 白名单按**解析后目标**的扩展名判定：真正被服务的文件决定它与 Content-Type。
    media_type = image_content_type(target)
    if media_type is None:
        raise HTTPException(status_code=404, detail="not an allowed image extension")

    if not await run_in_threadpool(target.is_file):
        raise HTTPException(status_code=404, detail="image not found")

    try:
        info = await run_in_threadpool(target.stat)
    except OSError:
        # is_file 与 stat 之间文件消失（TOCTOU）：对客户端而言就是"没有"。
        raise HTTPException(status_code=404, detail="image not found")
    if not stat.S_ISREG(info.st_mode):
        raise HTTPException(status_code=404, detail="not a regular file")
    if info.st_size > IMAGE_MAX_BYTES:
        raise HTTPException(
            status_code=413,
            detail=(
                f"image is {info.st_size} bytes, exceeds the "
                f"{IMAGE_MAX_BYTES} bytes limit"
            ),
        )

    try:
        data = await run_in_threadpool(target.read_bytes)
    except OSError:
        raise HTTPException(status_code=404, detail="image not found")
    if len(data) > IMAGE_MAX_BYTES:  # stat 与 read 之间的竞态兜底
        raise HTTPException(
            status_code=413,
            detail=f"image exceeds {IMAGE_MAX_BYTES} bytes",
        )

    headers = {
        # 同一路径的内容可能被重写（读图后覆盖同名文件即换图）：一律回源。
        "Cache-Control": CACHE_CONTROL_IMAGE,
        "X-Content-Type-Options": "nosniff",
    }
    if media_type == "image/svg+xml":
        # 直开 SVG 是一个文档上下文：禁脚本（<img> 引用时本就不执行脚本）。
        headers["Content-Security-Policy"] = "sandbox"
    return Response(content=data, media_type=media_type, headers=headers)
