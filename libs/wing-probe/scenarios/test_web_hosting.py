"""web 托管 probe 场景（step 03）：静态托管 + SPA fallback 的接口行为。

覆盖（spec a 组）：

1. 未配置 `static_dir`：`GET /` 与未知路径维持现状（404 + ErrorResponse 形状），
   `/api/*` 不受影响；
2. 配置 `static_dir`（**相对路径** `"static"` → `$WING_HOME/core/static`）：
   `index.html`（`no-cache`）、`/assets/*`（`immutable` + 正确 Content-Type + 原始字节）、
   未命中与目录命中都 SPA fallback、`/api/unknown` 与保留前缀永不 fallback、
   `/docs` / `/openapi.json` 仍是框架路由；
3. 配置的目录不存在 → 404（web 还没构建也不拖垮网关）；
4. URL 编码的 `../` 越界 → 403，且拿不到静态根之外的内容（用 `config.yaml` 里的
   provider key 作哨兵字符串）。

断言面是**原始 HTTP**（状态码 + 响应头 + 字节）：静态托管承诺的正是这些，
driver 的结构化 JSON 通道看不到。
"""

from __future__ import annotations

from pathlib import Path

import httpx
import pytest

from wing_probe import Probe

INDEX_HTML = "<!doctype html><html><body>wing web shell</body></html>"
APP_JS = "console.log('wing-asset');\n"
FAVICON = b"\x00\x01\x02favicon-bytes"

#: 相对 static_dir 的落点：配置所在目录（$WING_HOME/core）下的 `static/`。
STATIC_DIRNAME = "static"


def static_root(probe: Probe) -> Path:
    """`static_dir: "static"` 解析出的根目录（相对路径按 $WING_HOME/core 解析）。"""
    return probe.env.wing_home / "core" / STATIC_DIRNAME


def write_static(probe: Probe, name: str, content: str | bytes) -> Path:
    """往静态根里写一个文件（场景启动后写——目录后出现也要生效）。"""
    path = static_root(probe) / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content.encode("utf-8") if isinstance(content, str) else content)
    return path


async def get(http: httpx.AsyncClient, path: str) -> httpx.Response:
    return await http.get(path)


# ── 1. 未配置：维持现状 ──────────────────────────────────────────


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_static_hosting_disabled_keeps_current_404(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """未配置 `static_dir`：未知路径仍是 404，`/api/*` 与 `/docs` 不受影响。

    "维持现状"是这一步的硬要求：默认关闭意味着对既有部署零影响。
    """
    assert probe.env.gateway_url.startswith("http://127.0.0.1:")
    for path in ("/", "/index.html", "/deep/route", "/api/unknown", "/assets/app.js"):
        response = await get(raw_http, path)
        assert response.status_code == 404, (path, response.status_code)
        assert response.json()["error"] == "not_found", (path, response.text)
        assert response.json()["detail"] == "Not Found", (path, response.text)

    health = await get(raw_http, "/api/health")
    assert health.status_code == 200, health.text
    listing = await get(raw_http, "/api/session/list")
    assert listing.status_code == 200, listing.text
    docs = await get(raw_http, "/docs")
    assert docs.status_code == 200, docs.text


# ── 2. 配置后：命中文件 / SPA fallback / 保留前缀 ─────────────────


@pytest.mark.probe_env(gateway_extra={"static_dir": STATIC_DIRNAME})
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_static_hosting_serves_build_and_falls_back(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """index.html / 资产 / SPA fallback / 保留前缀的完整行为矩阵。"""
    write_static(probe, "index.html", INDEX_HTML)
    (static_root(probe) / "assets").mkdir(parents=True, exist_ok=True)
    write_static(probe, "assets/app.js", APP_JS)
    write_static(probe, "favicon.png", FAVICON)

    # ① 根路径 = index.html（no-cache：绝不拿旧壳）。
    root = await get(raw_http, "/")
    assert root.status_code == 200, root.text
    assert root.text == INDEX_HTML
    assert root.headers["content-type"].startswith("text/html")
    assert root.headers["cache-control"] == "no-cache"

    # ② /assets/*：内容哈希产物 → 一年不可变 + 正确类型 + 原始字节。
    asset = await get(raw_http, "/assets/app.js")
    assert asset.status_code == 200, asset.text
    assert asset.text == APP_JS
    assert asset.headers["content-type"].startswith("text/javascript")
    assert asset.headers["cache-control"] == "public, max-age=31536000, immutable"

    # ③ 非 assets 的静态文件：no-cache（只有 /assets/* 承诺不可变）。
    icon = await get(raw_http, "/favicon.png")
    assert icon.status_code == 200, icon.text
    assert icon.content == FAVICON
    assert icon.headers["content-type"] == "image/png"
    assert icon.headers["cache-control"] == "no-cache"

    # ④ SPA fallback：任何未命中的前端路由都回壳（含 // 与目录命中）。
    for path in (
        "/deep/link/route",
        "/settings/gateway",
        "/assets/missing-hash.js",
        "/assets",
    ):
        response = await get(raw_http, path)
        assert response.status_code == 200, (path, response.status_code)
        assert response.text == INDEX_HTML, (path, response.text[:80])
        assert response.headers["cache-control"] == "no-cache", path

    # ⑤ 保留前缀永不 fallback：API 拼错就是 404，不会被 200 的 HTML 吞掉。
    for path in ("/api", "/api/unknown", "/api/session/list-x", "/ws"):
        response = await get(raw_http, path)
        assert response.status_code == 404, (path, response.status_code, response.text)
        assert response.json()["error"] == "not_found", (path, response.text)

    # 前缀匹配不越界：`/apidocs`、`/openapi.json-x`、`/wsx` 都只是普通 SPA 路径。
    for path in ("/apidocs", "/openapi.json-x", "/wsx"):
        response = await get(raw_http, path)
        assert response.status_code == 200, (path, response.status_code, response.text)
        assert response.text == INDEX_HTML, path

    # ⑥ 框架路由仍是框架路由（不被 catch-all 吃掉）。
    openapi = await get(raw_http, "/openapi.json")
    assert openapi.status_code == 200, openapi.text
    assert openapi.headers["content-type"].startswith("application/json")
    assert "openapi" in openapi.json()
    docs = await get(raw_http, "/docs")
    assert docs.status_code == 200, docs.text
    assert "swagger" in docs.text.lower()

    # ⑦ 已知 API 端点照常工作（托管不改变 API 行为）。
    assert (await get(raw_http, "/api/health")).status_code == 200

    # ⑧ HEAD：静态托管的常规语义（头部照给、无 body）。
    head = await raw_http.head("/assets/app.js")
    assert head.status_code == 200, head.status_code
    assert head.content == b"", head.content[:40]
    assert head.headers["content-type"].startswith("text/javascript")
    assert head.headers["cache-control"] == "public, max-age=31536000, immutable"


@pytest.mark.probe_env(gateway_extra={"static_dir": STATIC_DIRNAME})
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_missing_static_dir_is_plain_404(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """配置指向**不存在**的目录：网关照常起、路径回 404（不报错、不拖垮网关）。"""
    configured = static_root(probe)
    assert not configured.exists(), "本场景假定目录未创建（web 还没构建）"

    for path in ("/", "/index.html", "/deep/route"):
        response = await get(raw_http, path)
        assert response.status_code == 404, (path, response.status_code)
        assert response.json()["error"] == "not_found", response.text

    assert (await get(raw_http, "/api/health")).status_code == 200

    # 目录事后出现即生效（逐请求解析，无需重启）——把"未构建"到"已构建"的
    # 过渡也钉住：这正是部署时 rsync 上去就能用的前提。
    write_static(probe, "index.html", INDEX_HTML)
    served = await get(raw_http, "/")
    assert served.status_code == 200, served.text
    assert served.text == INDEX_HTML


# ── 3. 越界：URL 编码不可绕过 ─────────────────────────────────────


@pytest.mark.probe_env(gateway_extra={"static_dir": STATIC_DIRNAME})
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_static_hosting_refuses_escapes(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """越界（含 URL 编码的 `../`）必须 403，且拿不到静态根之外的内容。

    哨兵用 `<wing_home>/core/config.yaml`：它就在静态根的**上一层**，内容里
    含有假 Provider 的 api key（`probe-key`）——任何"编码绕过"都会把它带出来。
    """
    write_static(probe, "index.html", INDEX_HTML)
    config_yaml = probe.env.config_path
    assert config_yaml.is_file()
    secret = config_yaml.read_text(encoding="utf-8")
    assert "probe-key" in secret, "哨兵字符串不在 config.yaml 里，断言失去意义"

    # ① 编码形态的穿越：`%2e%2e` / `%2f` 由 uvicorn 解码后进入同一套 realpath 解析
    #    → 403（编码不可绕过）。httpx 不重编码已有百分号，所以这些形态原样上线。
    for path in (
        "/%2e%2e/core/config.yaml",
        "/%2e%2e%2fcore%2fconfig.yaml",
        "/..%2fcore%2fconfig.yaml",
        "/assets/%2e%2e%2f%2e%2e%2fcore%2fconfig.yaml",
    ):
        response = await get(raw_http, path)
        assert response.status_code == 403, (path, response.status_code, response.text)
        assert "probe-key" not in response.text, (path, "越界内容泄露")

    # ② 绝对路径形态（编码后的前导 `/` 解码成 `//etc/hosts` → 绝对路径 → 越界）。
    for path in (
        "/%2Fetc%2Fhosts",
        "/%2fetc%2fhosts",
        "/%2e%2e%2f%2e%2e%2f%2e%2e%2fetc%2fpasswd",
    ):
        response = await get(raw_http, path)
        assert response.status_code == 403, (path, response.status_code, response.text)
        assert "localhost" not in response.text, (path, "越界内容泄露")

    # ③ 客户端自行归一化的形态（`//etc/hosts` → `/etc/hosts`、`/./../etc/hosts` →
    #    `/etc/hosts`）拿不到 root 外内容：它们落回根内解析 → 未命中 → SPA fallback。
    #    "不经 URL 编码绕过"这条红线在服务端侧由 ①② 覆盖。
    for path in ("//etc/hosts", "/./../etc/hosts"):
        response = await get(raw_http, path)
        assert response.status_code == 200, (path, response.status_code, response.text)
        assert response.text == INDEX_HTML, (path, response.text[:80])
        assert "localhost" not in response.text, (path, "越界内容泄露")

    # ④ 越界只针对越界路径：正常路径照常工作（403 不是"整站拒绝"）。
    assert (await get(raw_http, "/")).status_code == 200
