"""web 托管 probe 场景（step 03）：鉴权语义（静态资源公开 / API 维持现状）。

覆盖（spec b 组）：

1. auth 开启：静态资源免 key 可访问（浏览器要能加载 web 壳），`/api/*` 仍 401
   （会话列表与图片端点都算），框架路由（`/docs`）也仍要 key；带 key 一切可用；
2. auth 关闭：静态、API、图片端点全部可用（既有部署零影响）。

auth 开启的场景必须 `connect=False`：probe driver 的 WS 握手不带 key，会被
网关以 4001 拒绝——本场景要的正是"没有 key 的客户端"，因此业务面一律走
`raw_http`（未鉴权 + 带 `X-API-Key` 两种形态各取所需）。
"""

from __future__ import annotations

from pathlib import Path

import httpx
import pytest

from wing_probe import Probe

API_KEY = "probe-web-key"
STATIC_DIRNAME = "static"
INDEX_HTML = "<!doctype html><html><body>wing web shell</body></html>"


def static_root(probe: Probe) -> Path:
    """`static_dir: "static"` 解析出的根目录（相对路径按 $WING_HOME/core 解析）。"""
    return probe.env.wing_home / "core" / STATIC_DIRNAME


def write_static(probe: Probe, name: str, content: str) -> Path:
    path = static_root(probe) / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return path


AUTH_ENABLED = {
    "static_dir": STATIC_DIRNAME,
    "auth": {"enabled": True, "keys": [{"key": API_KEY, "role": "admin"}]},
}

AUTH_DISABLED = {"static_dir": STATIC_DIRNAME}


@pytest.mark.probe_env(gateway_extra=AUTH_ENABLED, connect=False)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_auth_enabled_keeps_api_protected_but_serves_shell(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """auth 开启：静态资源公开、API 与框架路由仍要 key。"""
    write_static(probe, "index.html", INDEX_HTML)
    write_static(probe, "assets/app.js", "console.log(1);")
    session_id = "whatever"

    # ① 静态资源：免 key 可访问（否则浏览器连壳都加载不了）。
    root = await raw_http.get("/")
    assert root.status_code == 200, root.text
    assert root.text == INDEX_HTML
    asset = await raw_http.get("/assets/app.js")
    assert asset.status_code == 200, asset.text
    spa = await raw_http.get("/deep/link")
    assert spa.status_code == 200, spa.text
    assert spa.text == INDEX_HTML

    # ② API 维持现状：无 key → 401（统一 ErrorResponse 形状）。
    listing = await raw_http.get("/api/session/list")
    assert listing.status_code == 401, listing.text
    assert listing.json()["error"] == "unauthorized", listing.text
    image = await raw_http.get(
        "/api/workspace/image",
        params={"session_id": session_id, "path": "pic.png"},
    )
    assert image.status_code == 401, image.text
    assert image.json()["error"] == "unauthorized", image.text
    health = await raw_http.get("/api/health")
    assert health.status_code == 200, health.text  # 健康检查始终豁免

    # ③ 框架路由不因"静态资源公开"被顺带放开。
    docs = await raw_http.get("/docs")
    assert docs.status_code == 401, docs.text
    openapi = await raw_http.get("/openapi.json")
    assert openapi.status_code == 401, openapi.text

    # ④ 带 key：API 可用；图片端点走到业务判定（不再 401）——会话不存在回 404，
    #    说明鉴权已过、请求真的进了端点逻辑。
    headers = {"X-API-Key": API_KEY}
    authed_listing = await raw_http.get("/api/session/list", headers=headers)
    assert authed_listing.status_code == 200, authed_listing.text
    authed_image = await raw_http.get(
        "/api/workspace/image",
        params={"session_id": session_id, "path": "pic.png"},
        headers=headers,
    )
    assert authed_image.status_code == 404, authed_image.text
    assert authed_image.json()["error"] == "not_found", authed_image.text
    authed_docs = await raw_http.get("/docs", headers=headers)
    assert authed_docs.status_code == 200, authed_docs.text

    # ⑤ 静态路径**完全不进鉴权判定**：带着错误的 key 也一样能拿到壳
    #    （浏览器加载资产时本就不带 key——豁免发生在校验之前）。
    bogus = await raw_http.get("/", headers={"Authorization": "Bearer not-the-key"})
    assert bogus.status_code == 200, bogus.text
    assert bogus.text == INDEX_HTML


@pytest.mark.probe_env(gateway_extra=AUTH_DISABLED)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_auth_disabled_serves_everything(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """auth 关闭（默认）：静态、API、图片端点全部可用（对既有部署零影响）。"""
    write_static(probe, "index.html", INDEX_HTML)

    root = await raw_http.get("/")
    assert root.status_code == 200, root.text
    listing = await raw_http.get("/api/session/list")
    assert listing.status_code == 200, listing.text
    # 图片端点：无鉴权直接进入业务判定（缺参 → 400，不是 401/403）。
    image = await raw_http.get("/api/workspace/image", params={"session_id": "x"})
    assert image.status_code == 400, image.text
    assert image.json()["error"] == "bad_request", image.text
    docs = await raw_http.get("/docs")
    assert docs.status_code == 200, docs.text
