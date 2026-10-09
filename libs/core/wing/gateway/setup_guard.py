# wing/gateway/setup_guard.py — setup mode 守门（配置不可用时的降级面）

"""setup mode（配置不可用）下 HTTP 面的守门：只放行设置端点，其余一律 503。

为什么需要它：网关降级启动后 ``server.runtime`` 只是一具「只服务保存事务」的替身
（见 ``server.py`` 的 ``_SetupRuntime``），任何依赖真 runtime 的端点都无法工作。
守门把「配置不可用」翻译成**协议级**的 503 + ``error="setup_mode"``（Rust 侧
``ApiClientError::is_setup_mode()`` 据此判定，见 protocol_addendum **P4**——
``HTTP_ERROR_TYPES[503]`` 是通用的 ``service_unavailable``，setup 语义**显式覆盖**，
否则所有 503 都会被看成 setup mode），并给出人类可读的修复指引。

**不是**鉴权层：修复模式的 loopback-only 判定住 ``auth.py``（``AuthMiddleware``
在中间件栈的最外层，先于本中间件——非 loopback 在 setup mode 下任何路径都先吃 403）。
本中间件只做「放行 or 503」。
"""

from __future__ import annotations

from collections.abc import Sequence

from starlette.middleware.base import BaseHTTPMiddleware, RequestResponseEndpoint
from starlette.requests import Request
from starlette.responses import Response
from starlette.types import ASGIApp

from wing.config import ConfigProblem

from .protocol import error_response

#: setup mode 下**可用**的路径：修复配置所需要的最小集合。
#: 其余一切（session / models / agents / commands / tools / system reload …）一律 503。
SETUP_ALLOWED_PATHS: set[str] = {
    "/api/health",
    "/api/settings/schema",
    "/api/settings/get",
    "/api/settings/status",
    "/api/settings/set",
    "/api/shutdown",
    "/openapi.json",
    "/docs",
    "/redoc",  # 修复期也要能查协议
}

#: ``render_setup_detail`` 最多列出的 problem 条数（与 cli.py 的降级横幅同一口径）。
SETUP_DETAIL_PROBLEM_LIMIT = 10


class SetupModeError(Exception):
    """setup mode 下访问 runtime。

    **刻意不是** ``ValueError`` / ``RuntimeError``：路由层把 ``ValueError`` 映射成 400、
    把 ``RuntimeError`` 映射成 400/500——「配置不可用」不是请求错误，是服务端状态。
    它是裸 ``Exception``，由 ``app.py`` 注册的异常处理器映射成
    ``503 + error="setup_mode"``（守门中间件已保证这些访问不可达，这是万一可达时的
    安全网：503 比 400/500 诚实）。
    """

    def __init__(self, problems: Sequence[ConfigProblem] = ()) -> None:
        super().__init__("gateway is in setup mode: the configuration is unusable")
        self.problems: list[ConfigProblem] = list(problems)


def render_setup_detail(problems: Sequence[ConfigProblem]) -> str:
    """503 的 ``detail``：前 10 条 problem + 修复指引（多行纯文本）。

    与 ``cli.py`` 的降级横幅同一批 problem、同一句指引——修复路径只有一条，
    说明也应该只有一套说法。
    """
    lines = [f"网关处于修复模式（配置不可用，共 {len(problems)} 条问题）："]
    for problem in problems[:SETUP_DETAIL_PROBLEM_LIMIT]:
        lines.append(f"- {problem.path or '<document>'}: {problem.message}")
    if len(problems) > SETUP_DETAIL_PROBLEM_LIMIT:
        lines.append(f"……另有 {len(problems) - SETUP_DETAIL_PROBLEM_LIMIT} 条问题。")
    lines.append("运行 `wing` 打开设置面板修复，或 `wing config doctor` 查看详情。")
    return "\n".join(lines)


class SetupGuardMiddleware(BaseHTTPMiddleware):
    """setup mode 的守门：白名单外的路径一律 ``503 error="setup_mode"``。

    注册顺序见 ``app.py``：本中间件在 ``AuthMiddleware`` **之内**（先 add ⇒ 更内层）
    ——修复模式的 loopback 判定必须**先**发生（§8.4）。
    """

    def __init__(self, app: ASGIApp) -> None:
        super().__init__(app)

    async def dispatch(
        self, request: Request, call_next: RequestResponseEndpoint
    ) -> Response:
        server = request.app.state.server
        if not server.in_setup_mode or request.url.path in SETUP_ALLOWED_PATHS:
            return await call_next(request)
        return error_response(
            503, render_setup_detail(server.setup_problems), error="setup_mode"
        )


__all__ = [
    "SETUP_ALLOWED_PATHS",
    "SETUP_DETAIL_PROBLEM_LIMIT",
    "SetupGuardMiddleware",
    "SetupModeError",
    "render_setup_detail",
]
