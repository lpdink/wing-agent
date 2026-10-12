# wing/gateway/cors.py — 跨源访问（CORS）装配

"""跨源访问（CORS）装配：默认放通 loopback 来源，外部来源走显式配置。

部署口径：**推荐同源部署**——前端静态文件与 API 走同一个反向代理（同源），该形态
下 CORS 完全不被用到、网关零配置。默认放通 loopback 来源（``http(s)://localhost`` /
``127.0.0.1`` / ``[::1]``，任意端口）只服务本机开发便利与边缘场景；异机前端是
**跨源**的，需要把来源写进 ``gateway.cors.origins``。WebSocket（``/ws``）不受 CORS
约束，不需要也不提供任何跨源处理。

安全姿态：能过鉴权即全权（反代 / HTTPS / 应用层鉴权在 CORS 之前）。放开 loopback
来源不构成新权限——本机进程本就不受浏览器同源策略约束（直连即可），且非 loopback
页面不受影响。``allow_credentials`` 恒 ``False``：API key 走 header，不用 cookie。
"""

from __future__ import annotations

from fastapi import FastAPI
from starlette.middleware.cors import CORSMiddleware

#: 默认放通的 loopback 来源：http/https + localhost / 127.0.0.1 / [::1] + 任意端口。
#: CORS 规范不支持端口通配，所以用 host 谓词匹配、按请求回声具体 origin
#: （``CORSMiddleware`` 的 ``allow_origin_regex`` 即此语义）。
LOOPBACK_ORIGIN_REGEX = r"^https?://(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$"

#: 跨源请求允许的方法 / 请求头（前端最小需求，预检要能应答）。
CORS_METHODS = ["GET", "POST", "HEAD", "OPTIONS"]
CORS_HEADERS = ["Authorization", "Content-Type", "X-API-Key", "X-Client-Id"]

#: 跨源前端能读到的响应头：Fetch 规范的 safelist 只有 Cache-Control / Content-Language /
#: Content-Length / Content-Type / Expires / Last-Modified / Pragma——`read` 的 Range /
#: inline 文件名素材（Content-Range / Content-Disposition / ETag / Accept-Ranges）
#: 必须显式 expose，否则 JS 里读到 null。
CORS_EXPOSE_HEADERS = [
    "Content-Range",
    "Content-Disposition",
    "ETag",
    "Accept-Ranges",
]


def install_cors(app: FastAPI, origins: list[str]) -> None:
    """装配 CORS 中间件（**必须最外层**：预检不带凭据，不能先被鉴权拦下）。

    ``origins`` 来自 ``gateway.cors.origins``——app 装配时读一次，属 **restart 域**
    （改完要重启网关；见 ``docs/dev/http-api.md``）。
    """
    app.add_middleware(
        CORSMiddleware,
        allow_origins=list(origins),
        allow_origin_regex=LOOPBACK_ORIGIN_REGEX,
        allow_credentials=False,
        allow_methods=CORS_METHODS,
        allow_headers=CORS_HEADERS,
        expose_headers=CORS_EXPOSE_HEADERS,
    )
