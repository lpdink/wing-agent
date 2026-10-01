# wing/gateway/routes/static.py — web 构建的静态托管 + SPA fallback

"""`gateway.static_dir` 的托管路由（默认关闭）。

**必须是最后注册的路由**：它是一个 catch-all（`/{file_path:path}`），先注册的真实
路由按注册序优先命中，只有没人认领的路径才落到这里。

**匹配层门控**（`StaticHostRoute.matches`，本文件的核心设计）：catch-all 只认领
「非保留前缀 + GET/HEAD」。保留前缀与其它方法在匹配阶段就返回 `Match.NONE`，
于是 Starlette 自己的三个既有分支原样生效——这是"默认配置（`static_dir=null`）下
既有行为零回归"的结构性保证，而不是在 handler 里补写一遍这些语义：

- 尾随斜杠重定向（`GET /docs/`、`GET /api/health/` → **307** → 去尾斜杠的路径）：
  Starlette 的 `redirect_slashes` 分支只在"没有任何 FULL/PARTIAL 匹配"时才跑，
  catch-all 若认领这些路径就永远轮不到它；
- 方法不允许（`HEAD /api/health` → **405** + `Allow`，由真实路由的 PARTIAL 匹配给出）；
- 未知路径的 404（`POST /api/unknown` 这类非 GET/HEAD 请求根本不该被 catch-all 认领，
  也就不会把今天的 404 变成 405）。

handler 行为：

- 命中普通文件 → 原样返回（显式 Content-Type 表；落在 `<root>/assets/**` 的产物
  不可变缓存，其余 `no-cache`；`stat_result` 由本模块统一取得后交给 `FileResponse`）；
- 未命中（含目录命中、名字超长、竞态消失）→ SPA fallback 返回 `index.html`
  （同样是 `no-cache`）；`index.html` 也没有 → 404；
- 越界 / 坏路径 → 403（与工作区图片端点同口径：越界必须显式 403）；
- 存在但本进程读不了 → 403（不让 `FileResponse` 抛出 500 + traceback）；
- 未配置 `static_dir` 或目录不存在 → 404（维持今天的未知路径语义）。
"""

from __future__ import annotations

import os
from pathlib import Path

from fastapi import APIRouter, HTTPException, Request
from fastapi.routing import APIRoute
from starlette.responses import FileResponse, Response
from starlette.routing import Match, get_route_path
from starlette.types import Scope

from wing.gateway.file_policy import (
    BadPathError,
    PathOutsideRootError,
    cache_control_for,
    resolve_within_root,
    safe_file_stat,
    static_content_type,
)
from wing.gateway.static_host import is_reserved_path, resolve_static_root

#: SPA 入口文件名（fallback 与根路径都用它）。
INDEX_FILE = "index.html"

#: catch-all 服务的方法（其它方法不认领，让 405 / 404 的既有语义回到 Starlette）。
_SERVED_METHODS = frozenset({"GET", "HEAD"})


class StaticHostRoute(APIRoute):
    """静态托管 catch-all 的路由类：保留前缀与非 GET/HEAD 在匹配层即不认领。"""

    def matches(self, scope: Scope) -> tuple[Match, Scope]:
        if scope["type"] == "http":
            # 不认领时返回 NONE（而不是 PARTIAL）：PARTIAL 会被 Starlette 当成
            # "方法不允许"直接回 405，而这里要的恰恰是让 405 / 404 / 307 的既有
            # 分支自己跑（未知路径的 POST 今天是 404，不是 405）。
            # `get_route_path` 而不是 `scope["path"]`：反代挂子路径（root_path）时
            # 路由匹配用的是剥掉前缀的路径，保留前缀判定必须同一个口径，否则
            # `/prefix/api/...` 会被当成普通路径 fallback 成壳。
            route_path = get_route_path(scope)
            method = str(scope.get("method", ""))
            if is_reserved_path(route_path) or method not in _SERVED_METHODS:
                return Match.NONE, {}
        return super().matches(scope)


router = APIRouter(route_class=StaticHostRoute)


def _not_found() -> HTTPException:
    """与 Starlette 兜底 404 同形状（`{"error": "not_found", "detail": "Not Found"}`）。"""
    return HTTPException(status_code=404, detail="Not Found")


def _serve_file(
    target: Path, *, static_root: Path, info: os.stat_result
) -> FileResponse:
    """返回一个静态文件（Content-Type / 缓存头 / 可读性都已判定）。

    `stat_result` 由调用方传入（`FileResponse` 因此不再自己 stat）：既省一次系统
    调用，也消除"检查通过 → open 前文件消失"时 Starlette 抛 `RuntimeError` 变成
    500 的窗口（rsync / 部署中途改名是真实场景）。
    """
    if not os.access(target, os.R_OK):
        # 存在但读不了：静态内容是公开的，问题在服务端权限配置——给 403，而不是
        # 让 FileResponse 抛出（那会变成"200 + 空 body + traceback"）。
        raise HTTPException(status_code=403, detail="static file is not readable")
    return FileResponse(
        target,
        media_type=static_content_type(target),
        stat_result=info,
        headers={
            # 只有落在 <root>/assets/** 的产物（内容哈希命名）才承诺不可变；
            # 其余（含 index.html）一律 no-cache。
            "Cache-Control": cache_control_for(target, static_root=static_root),
            # 类型由扩展名表决定，禁止浏览器再嗅探（防 .txt 被当作 HTML 执行）。
            "X-Content-Type-Options": "nosniff",
        },
    )


def _fallback(root: Path) -> FileResponse:
    """SPA fallback：`index.html`；取不到 stat / 非普通文件即 404（未构建 / 构建不完整）。"""
    index = root / INDEX_FILE
    info = safe_file_stat(index)
    if info is None:
        raise _not_found()
    return _serve_file(index, static_root=root, info=info)


@router.api_route(
    "/{file_path:path}",
    methods=sorted(_SERVED_METHODS),
    include_in_schema=False,
    response_model=None,
)
async def serve_static(file_path: str, request: Request) -> Response:
    """静态托管 / SPA fallback（见模块文档）。"""
    # 匹配层已挡住保留路径（这里是不变量护栏：万一有人把路由类换回普通 APIRoute，
    # 至少不会把 API 路径 fallback 成 200 的 HTML）。
    if is_reserved_path(get_route_path(request.scope)):
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

    info = safe_file_stat(target)
    # 不是普通文件（目录命中 / 名字超长 / 检查后被删）→ SPA fallback。
    if info is not None:
        return _serve_file(target, static_root=root, info=info)
    return _fallback(root)
