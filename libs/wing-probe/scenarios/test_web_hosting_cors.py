"""web 托管 probe 场景（step 03）：开发期 CORS 的放行边界。

覆盖（spec c 组）：

1. `cors_origins` 为空（默认）：**完全不放宽**——带 Origin 的简单请求没有
   `Access-Control-Allow-Origin`，预检被拒；
2. 非空：所列 origin 的简单请求与预检都通过（含 `Authorization` / `X-API-Key`
   这类鉴权头的回显——浏览器对 `Authorization` 不吃 `*` 通配），非所列 origin
   拿不到 ACAO（浏览器会拦），预检直接 400；
3. **auth × CORS 组合**（中间件顺序的全部意义所在）：auth 开启时，不带 key 的
   预检仍要在最外层被应答（200 + ACAO），而 401 响应也要带 ACAO，否则浏览器读不到
   "你没带 key"这个事实。把 `add_middleware` 顺序写反的实现会在这里变红。

断言看的是**响应头**（浏览器唯一依据），不是网关自己的日志。
"""

from __future__ import annotations

import httpx
import pytest

from wing_probe import Probe

ALLOWED_ORIGIN = "http://localhost:5173"
OTHER_ORIGIN = "http://evil.example"
API_KEY = "probe-cors-key"
STATIC_DIRNAME = "static"
INDEX_HTML = "<!doctype html><html><body>wing web shell</body></html>"

#: 预检请求的三件套（浏览器发的就是这个形状）。
PREFLIGHT_HEADERS = {
    "Origin": ALLOWED_ORIGIN,
    "Access-Control-Request-Method": "POST",
    "Access-Control-Request-Headers": "authorization,content-type,x-client-id",
}


def _cors(response: httpx.Response) -> dict[str, str]:
    """响应里的 CORS 相关头（小写键）。"""
    return {
        key.lower(): value
        for key, value in response.headers.items()
        if key.lower().startswith("access-control-")
    }


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_cors_disabled_by_default(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """空 `cors_origins`：不放宽任何跨源访问（默认关闭的硬要求）。"""
    assert probe.env.gateway_url.startswith("http://127.0.0.1:")

    simple = await raw_http.get("/api/health", headers={"Origin": ALLOWED_ORIGIN})
    assert simple.status_code == 200, simple.text
    assert _cors(simple) == {}, _cors(simple)

    preflight = await raw_http.options("/api/session/list", headers=PREFLIGHT_HEADERS)
    assert preflight.status_code >= 400, preflight.status_code
    assert "access-control-allow-origin" not in _cors(preflight), _cors(preflight)


@pytest.mark.probe_env(gateway_extra={"cors_origins": [ALLOWED_ORIGIN]})
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_cors_allows_only_listed_origin(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """非空 `cors_origins`：只放行所列 origin（简单请求 + 预检两条路径）。"""
    assert probe.env.gateway_url.startswith("http://127.0.0.1:")

    # ① 简单请求：所列 origin 拿到 ACAO（且回显具体 origin，不是 `*`）。
    allowed = await raw_http.get("/api/health", headers={"Origin": ALLOWED_ORIGIN})
    assert allowed.status_code == 200, allowed.text
    assert allowed.headers["access-control-allow-origin"] == ALLOWED_ORIGIN

    # ② 非所列 origin：没有 ACAO，浏览器侧直接拦截（网关仍正常回业务响应）。
    denied = await raw_http.get("/api/health", headers={"Origin": OTHER_ORIGIN})
    assert denied.status_code == 200, denied.text
    assert "access-control-allow-origin" not in _cors(denied), _cors(denied)

    # ③ 预检：所列 origin 通过，方法/请求头按请求回显（含 Authorization——
    #    浏览器对 `Authorization` 不吃 `*` 通配，必须点名）。
    preflight = await raw_http.options("/api/session/list", headers=PREFLIGHT_HEADERS)
    assert preflight.status_code == 200, preflight.text
    headers = _cors(preflight)
    assert headers["access-control-allow-origin"] == ALLOWED_ORIGIN
    assert "POST" in headers["access-control-allow-methods"]
    requested = headers["access-control-allow-headers"].lower()
    assert "authorization" in requested, headers
    assert "x-client-id" in requested, headers
    # 不开 credentials：鉴权走显式请求头，不用 Cookie。
    assert headers.get("access-control-allow-credentials") is None, headers

    # ④ 非所列 origin 的预检：直接 400，不放行。
    bad_preflight = await raw_http.options(
        "/api/session/list",
        headers={**PREFLIGHT_HEADERS, "Origin": OTHER_ORIGIN},
    )
    assert bad_preflight.status_code == 400, bad_preflight.text
    assert "access-control-allow-origin" not in _cors(bad_preflight)


# ── 3. auth × CORS：中间件顺序的不变量 ────────────────────────────


@pytest.mark.probe_env(
    connect=False,  # driver 的 WS 无 key 会被拒连；本场景只要未鉴权的原始客户端
    gateway_extra={
        "static_dir": STATIC_DIRNAME,
        "cors_origins": [ALLOWED_ORIGIN],
        "auth": {"enabled": True, "keys": [{"key": API_KEY, "role": "admin"}]},
    },
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_cors_outermost_under_auth(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """auth 开启时 CORS 必须仍在最外层：预检免 key、401 响应带 ACAO。

    这是 `add_middleware` 顺序（CORS 后加 = 更外层）的守门测试：顺序写反时，
    预检会被 AuthMiddleware 拦成 401（浏览器直接判 CORS 失败），开发期跨源全线
    不可用——而这条链路上没有任何别的测试会变红。
    """
    static_dir = probe.env.wing_home / "core" / STATIC_DIRNAME
    static_dir.mkdir(parents=True, exist_ok=True)
    (static_dir / "index.html").write_text(INDEX_HTML, encoding="utf-8")

    # ① 预检（无 key）：最外层应答 200 + ACAO + 请求头回显（含 Authorization）。
    preflight = await raw_http.options(
        "/api/session/list",
        headers={
            "Origin": ALLOWED_ORIGIN,
            "Access-Control-Request-Method": "POST",
            "Access-Control-Request-Headers": "authorization,content-type,x-client-id",
        },
    )
    assert preflight.status_code == 200, preflight.text
    headers = _cors(preflight)
    assert headers["access-control-allow-origin"] == ALLOWED_ORIGIN, headers
    assert "authorization" in headers["access-control-allow-headers"].lower(), headers

    # ② 未带 key 的实际请求：401 且**带 ACAO**（浏览器读得到"缺 key"，而不是
    #    被 CORS 拦成不可读的网络错误）。
    denied = await raw_http.get("/api/session/list", headers={"Origin": ALLOWED_ORIGIN})
    assert denied.status_code == 401, denied.text
    assert denied.headers["access-control-allow-origin"] == ALLOWED_ORIGIN, dict(
        denied.headers
    )

    # ③ 带 key：200 + ACAO（鉴权通过后响应仍可被跨源页面读取）。
    allowed = await raw_http.get(
        "/api/session/list",
        headers={"Origin": ALLOWED_ORIGIN, "X-API-Key": API_KEY},
    )
    assert allowed.status_code == 200, allowed.text
    assert allowed.headers["access-control-allow-origin"] == ALLOWED_ORIGIN

    # ④ 非所列 origin 的预检：400 且无 ACAO（auth 开启不改变这条）。
    bad = await raw_http.options(
        "/api/session/list",
        headers={
            "Origin": OTHER_ORIGIN,
            "Access-Control-Request-Method": "POST",
        },
    )
    assert bad.status_code == 400, bad.text
    assert "access-control-allow-origin" not in _cors(bad), _cors(bad)

    # ⑤ 静态壳：公开（免 key）且同样带 ACAO（dev 模式下 web 壳从 vite server 侧
    #    访问时也要能读）。
    shell = await raw_http.get("/index.html", headers={"Origin": ALLOWED_ORIGIN})
    assert shell.status_code == 200, shell.text
    assert shell.text == INDEX_HTML
    assert shell.headers["access-control-allow-origin"] == ALLOWED_ORIGIN
