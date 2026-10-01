"""web 托管 probe 场景（step 03）：开发期 CORS 的放行边界。

覆盖（spec c 组）：

1. `cors_origins` 为空（默认）：**完全不放宽**——带 Origin 的简单请求没有
   `Access-Control-Allow-Origin`，预检被拒；
2. 非空：所列 origin 的简单请求与预检都通过（含 `Authorization` / `X-API-Key`
   这类鉴权头的回显——浏览器对 `Authorization` 不吃 `*` 通配），非所列 origin
   拿不到 ACAO（浏览器会拦），预检直接 400。

断言看的是**响应头**（浏览器唯一依据），不是网关自己的日志。
"""

from __future__ import annotations

import httpx
import pytest

from wing_probe import Probe

ALLOWED_ORIGIN = "http://localhost:5173"
OTHER_ORIGIN = "http://evil.example"

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
