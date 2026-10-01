# wing/gateway/routes/static.py — web 构建的静态托管 + SPA fallback

"""`gateway.static_dir` 的托管路由（默认关闭）。

**必须是最后注册的路由**：它是一个 catch-all（`/{file_path:path}`），先注册的真实
路由按注册序优先命中，只有没人认领的路径才落到这里。

行为：

- 保留前缀（`/api`、`/ws`、`/docs`、`/redoc`、`/openapi.json` 及其子路径）→ 404，
  **永不** fallback（否则 SPA 会用 200 的 HTML 吞掉 API 的拼写错误）；
- 命中普通文件 → 原样返回（显式 Content-Type 表；`/assets/*` 不可变缓存，其余
  `no-cache`；`FileResponse` 自带 ETag / Last-Modified / Range）；
- 未命中（含目录命中）→ SPA fallback 返回 `index.html`（同样是 `no-cache`）；
- 越界 / 坏路径 → 403（与工作区图片端点同口径：越界必须显式 403）；
- 未配置 `static_dir` 或目录不存在 → 404（维持今天的未知路径语义）。

只服务 GET / HEAD：其他方法打到未知路径由 Starlette 归为 405（路径被本路由认领、
方法不在其列表里），已知 API 路径的 405 语义不变。
"""

from __future__ import annotations

from pathlib import Path

from fastapi import APIRouter, HTTPException, Request
from starlette.responses import FileResponse, Response

from wing.gateway.file_policy import (
    BadPathError,
    PathOutsideRootError,
    cache_control_for,
    resolve_within_root,
    static_content_type,
)
from wing.gateway.static_host import is_reserved_path, resolve_static_root

router = APIRouter()

#: SPA 入口文件名（fallback 与根路径都用它）。
INDEX_FILE = "index.html"


def _serve_file(target: Path, *, request_path: str) -> FileResponse:
    """返回一个静态文件（Content-Type 与缓存头按请求路径判定）。"""
    return FileResponse(
        target,
        media_type=static_content_type(target),
        headers={
            "Cache-Control": cache_control_for(request_path),
            # 类型由扩展名表决定，禁止浏览器再嗅探（防 .txt 被当作 HTML 执行）。
            "X-Content-Type-Options": "nosniff",
        },
    )


def _not_found() -> HTTPException:
    """与 Starlette 兜底 404 同形状（`{"error": "not_found", "detail": "Not Found"}`）。"""
    return HTTPException(status_code=404, detail="Not Found")


def _fallback(root: Path) -> FileResponse:
    """SPA fallback：`index.html`；缺失即 404（未构建 / 构建不完整）。"""
    index = root / INDEX_FILE
    if not index.is_file():
        raise _not_found()
    return _serve_file(index, request_path=INDEX_FILE)


@router.api_route(
    "/{file_path:path}",
    methods=["GET", "HEAD"],
    include_in_schema=False,
    response_model=None,
)
async def serve_static(file_path: str, request: Request) -> Response:
    """静态托管 / SPA fallback（见模块文档）。"""
    if is_reserved_path(request.url.path):
        raise _not_found()

    root = resolve_static_root(request.app.state.server.gateway_config)
    if root is None:
        raise _not_found()

    if not file_path:
        return _fallback(root)  # 根路径 = SPA 入口

    try:
        target = resolve_within_root(root, file_path)
    except PathOutsideRootError:
        raise HTTPException(status_code=403, detail="path escapes the static root")
    except BadPathError:
        raise _not_found()

    if target.is_file():
        return _serve_file(target, request_path=file_path)
    # 未命中（含目录命中）：SPA 路由一律回壳，由前端路由自己渲染 404。
    return _fallback(root)
