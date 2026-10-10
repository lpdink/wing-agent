# wing/gateway/auth.py — API Key 鉴权

"""Gateway API Key 鉴权模块。

提供：
  - extract_key_from_headers: 从 HTTP headers 提取 API key
  - extract_key_from_ws: 从 WebSocket 连接提取 API key（headers + query param）
  - AuthMiddleware: Starlette HTTP 中间件，拦截未鉴权请求

鉴权逻辑完全在 Gateway 层，不侵入 Runtime。
AuthMiddleware 不缓存 AuthConfig——每次请求从 ``app.state.server.auth_config``
动态读取，确保 ``/api/system/reload`` 热重载后立即生效。

**修复模式（setup mode）**：配置坏掉时 auth 配置本身不可信，``dispatch`` 开头有一个
独立分支——只接受 loopback 来源、**且网关自身必须绑定在 loopback 上**（``LOOPBACK_HOSTS``），
两者都成立才不要求 key；否则一律 403。这是收紧不是放松（正常模式下 auth 关闭时任何人都能访问）。
第二个条件关掉的是「任何 loopback 转发者即修复者」：绑 ``0.0.0.0`` 且配置坏掉时，
本机反代 / 端口转发会让远端流量以 loopback 来源到达——那种部署不提供免 key 修复访问。
既有鉴权逻辑（enabled / EXEMPT_PATHS / key 校验 / RBAC）在 setup 分支之外**一字未改**。
"""

from __future__ import annotations

from collections.abc import Mapping

from fastapi import WebSocket
from starlette.middleware.base import BaseHTTPMiddleware, RequestResponseEndpoint
from starlette.requests import Request
from starlette.responses import JSONResponse, Response
from starlette.types import ASGIApp

from wing.gateway.protocol import error_response

# 免鉴权路径——无论 auth.enabled 如何，这些路径始终开放。
EXEMPT_PATHS: set[str] = {"/api/health"}

# loopback 主机名：**修复模式**（setup mode）免 key 的判据。HTTP 侧以
# ``request.client.host`` 判定，WS 侧同义复用（``websocket.client.host``）。
# 三种拼写覆盖 uvicorn 在本机监听时的全部常见来源（IPv4 / IPv6 / 名字）。
LOOPBACK_HOSTS: frozenset[str] = frozenset({"127.0.0.1", "::1", "localhost"})

# ── 身份角色 ─────────────────────────────────────────────────
# admin（ApiKeyEntry.role 默认值）：全量访问，是隐式的"非受限"角色。
# tool_runtime：纯工具执行远端，仅允许注册工具——唯一需要显式判定的角色。
# 既要注册工具又要订阅事件的客户端应持 admin 身份。
ROLE_TOOL_RUNTIME = "tool_runtime"

# tool_runtime 角色允许访问的路径（allowlist）。新增端点默认对其关闭——
# 安全默认，无需为每个新端点额外维护拒绝逻辑。/api/health 已由 EXEMPT_PATHS
# 在更早处放行，列入此处仅为语义完整。
TOOL_RUNTIME_ALLOWED_PATHS: set[str] = {"/api/tools/register", "/api/health"}


def _unauthorized() -> JSONResponse:
    """构造 401 响应（每次新建，避免共享实例被 middleware 链 mutate）。

    经 protocol.error_response 输出统一 ErrorResponse 形状，使鉴权失败也能被
    wing-api-client 结构化解析（中间件在 ExceptionMiddleware 之外，无法依赖
    gateway 的 exception handler）。
    """
    return error_response(401, "Invalid or missing API key")


def _forbidden() -> JSONResponse:
    """构造 403 响应（角色权限不足），与 401 同为统一 ErrorResponse 形状。"""
    return error_response(403, "Role not permitted to access this endpoint")


def extract_key_from_headers(headers: Mapping[str, str]) -> str | None:
    """从 HTTP headers 提取 API key。

    优先级：Authorization: Bearer <key> > X-API-Key: <key>。
    header name 查找不区分大小写（Starlette Headers 本身即如此）。
    """
    auth = headers.get("authorization", "")
    if auth.lower().startswith("bearer "):
        token = auth[7:].strip()
        if token:
            return token

    api_key = headers.get("x-api-key", "")
    if api_key:
        return api_key

    return None


def extract_key_from_ws(ws: WebSocket) -> str | None:
    """从 WebSocket 连接提取 API key。

    优先级：headers（同 HTTP）> query param ``api_key``。
    """
    key = extract_key_from_headers(ws.headers)
    if key:
        return key

    return ws.query_params.get("api_key") or None


class AuthMiddleware(BaseHTTPMiddleware):
    """HTTP API Key 鉴权中间件。

    当 ``auth_config.enabled`` 为 True 时，拦截所有非免鉴权路径的
    HTTP 请求，校验 API Key；RBAC 判定在中间件内部完成。

    不缓存 AuthConfig——每次 dispatch 从 ``app.state.server.auth_config``
    读取最新配置，热重载后立即生效。
    """

    def __init__(self, app: ASGIApp) -> None:
        super().__init__(app)

    async def dispatch(
        self, request: Request, call_next: RequestResponseEndpoint
    ) -> Response:
        server = request.app.state.server

        # 修复模式（setup mode）：配置坏掉 ⇒ auth 配置本身不可信（读不出来）。
        # 修复模式 ≈ 本地控制台访问：**只接受 loopback，不要求 key**。
        # 这是**收紧不是放松**——正常模式下 auth.enabled=false 时任何人都能访问，
        # setup mode 下只有本机能（哪怕 auth.enabled=false）。判定必须在读
        # auth 配置之前（那份配置此刻不可用）。
        #
        # **免 key 仅当网关自身绑定 loopback**：只看来访地址不够——
        # 任何把流量从 127.0.0.1 转发进来的本机进程（无鉴权反代 / 容器 sidecar /
        # 本地端口转发）都会让远端流量以 loopback 身份到达，而 setup mode 授予的是
        # **免 key 的整份配置写权限**。绑定地址取自 ``GatewayServer.host``（构造参数，
        # 已经过「文件里的值优先」解析）——**不在这里重读 config**（它正是不可信的那份）。
        if server.in_setup_mode:
            client_host = request.client.host if request.client else ""
            if client_host not in LOOPBACK_HOSTS:
                return error_response(
                    403,
                    "gateway is in setup mode: only loopback clients may read or "
                    "repair the configuration",
                )
            if server.host not in LOOPBACK_HOSTS:
                # 绑到非 loopback（0.0.0.0 / :: / 具体外部地址）⇒ 不提供免 key 访问。
                # 这里**不是**「要求一把 key」：setup mode 下 auth 配置不可信
                # （``server.auth_config`` 恒为安全默认、没有可核验的 keys），
                # 放行一把无法核验的 key 会把写面重新打开——直接拒绝才是安全分支。
                return error_response(
                    403,
                    "gateway is in setup mode: it is bound to a non-loopback "
                    f"address ({server.host}) — keyless repair access is not offered; "
                    "set gateway.host back to 127.0.0.1 and restart, or edit "
                    "config.yaml directly",
                )
            return await call_next(request)

        auth_config = server.auth_config

        if not auth_config.enabled:
            return await call_next(request)

        if request.url.path in EXEMPT_PATHS:
            return await call_next(request)

        key = extract_key_from_headers(request.headers)
        if key is None:
            return _unauthorized()

        role = auth_config.verify(key)
        if role is None:
            return _unauthorized()

        # RBAC：tool_runtime 仅允许访问 allowlist 路径，其余 403。
        # admin 全量放行；auth 关闭时本方法已在更早处早返回，不做强制。
        if (
            role == ROLE_TOOL_RUNTIME
            and request.url.path not in TOOL_RUNTIME_ALLOWED_PATHS
        ):
            return _forbidden()

        return await call_next(request)
