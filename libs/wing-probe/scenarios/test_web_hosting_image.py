"""受限工作区图片端点 probe 场景（step 03，接口冻结）：`GET /api/workspace/image`。

覆盖（spec d 组）：

- 200：相对路径 / 绝对路径（落在 workspace 内）→ 原始字节 + 正确 Content-Type
  （逐个扩展名钉住白名单映射）；10 MiB 边界（恰好 10 MiB 放行、超过即 413）；
- 400：缺参 / 空参 / NUL；
- 403：`..` 穿越（含 URL 编码形态）、符号链接逃逸、跨会话 workspace 隔离；
- 404：会话不在内存、白名单外扩展名、文件不存在、目录；
- 不实现 Range：带 `Range` 头仍 200 全量（没有 206、没有 Accept-Ranges）。

断言面：状态码 + 响应头 + **原始字节**（图片端点的契约就是这三样），
因此走 `raw_http` 而不是 driver 的结构化 JSON 通道。会话 / 文件都是真的
（driver 建会话拿 workspace，场景往里面写文件）。
"""

from __future__ import annotations

import base64
from pathlib import Path
from typing import Any

import httpx
import pytest

from wing_probe import Probe

ENDPOINT = "/api/workspace/image"

#: 最小合法 PNG（签名 + 可辨识内容），足以断言"字节原样返回"。
PNG_BYTES = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/"
    "q842iQAAAABJRU5ErkJggg=="
)

#: 扩展名 → 期望 Content-Type（与 `extensions/vscode/src/host/images.ts` 同口径）。
CONTENT_TYPES: dict[str, str] = {
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".gif": "image/gif",
    ".webp": "image/webp",
    ".svg": "image/svg+xml",
    ".bmp": "image/bmp",
    ".ico": "image/x-icon",
    ".avif": "image/avif",
    ".apng": "image/apng",
}

MIB = 1024 * 1024


async def image(
    http: httpx.AsyncClient,
    session_id: str,
    path: str,
    **kwargs: Any,
) -> httpx.Response:
    """打图片端点（params 走 httpx 编码；原始编码形态见调用方直接给 URL）。"""
    return await http.get(
        ENDPOINT, params={"session_id": session_id, "path": path}, **kwargs
    )


async def new_session(probe: Probe, name: str) -> tuple[str, Path]:
    """建一个带独立 workspace 的会话，返回 (session_id, workspace)。"""
    workspace = probe.env.root / "workspaces" / name
    session = await probe.session(workspace=workspace)
    return session.session_id, session.workspace or workspace


# ── 200：正常读取 ────────────────────────────────────────────────


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_reads_workspace_images(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """相对 / 绝对路径都能读，字节原样、Content-Type 按扩展名。"""
    session_id, workspace = await new_session(probe, "alpha")
    (workspace / "sub").mkdir(parents=True, exist_ok=True)
    (workspace / "pic.png").write_bytes(PNG_BYTES)
    (workspace / "sub" / "deep.png").write_bytes(PNG_BYTES)

    relative = await image(raw_http, session_id, "pic.png")
    assert relative.status_code == 200, relative.text
    assert relative.content == PNG_BYTES
    assert relative.headers["content-type"] == "image/png"
    assert relative.headers["x-content-type-options"] == "nosniff"
    assert relative.headers["cache-control"] == "no-store"
    assert "accept-ranges" not in relative.headers, dict(relative.headers)

    nested = await image(raw_http, session_id, "sub/deep.png")
    assert nested.status_code == 200, nested.text
    assert nested.content == PNG_BYTES

    absolute = await image(raw_http, session_id, str(workspace / "pic.png"))
    assert absolute.status_code == 200, absolute.text
    assert absolute.content == PNG_BYTES

    # 大写的扩展名同样在名单内（判定小写化）。
    (workspace / "UPPER.PNG").write_bytes(PNG_BYTES)
    upper = await image(raw_http, session_id, "UPPER.PNG")
    assert upper.status_code == 200, upper.text
    assert upper.headers["content-type"] == "image/png"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_content_type_matrix(probe: Probe, raw_http: httpx.AsyncClient) -> None:
    """白名单内每个扩展名都给出正确 Content-Type（含 svg 的加固头）。"""
    session_id, workspace = await new_session(probe, "types")
    for suffix in CONTENT_TYPES:
        (workspace / f"img{suffix}").write_bytes(PNG_BYTES)

    for suffix, expected in CONTENT_TYPES.items():
        response = await image(raw_http, session_id, f"img{suffix}")
        assert response.status_code == 200, (
            suffix,
            response.status_code,
            response.text,
        )
        assert response.headers["content-type"] == expected, suffix
        assert response.content == PNG_BYTES, suffix

    # SVG：直开即文档上下文 → 额外禁脚本（<img> 引用时本就不执行脚本）。
    svg = await image(raw_http, session_id, "img.svg")
    assert svg.headers["content-security-policy"] == "sandbox", dict(svg.headers)


# ── 400：缺参 / 坏参 ─────────────────────────────────────────────


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_missing_or_bad_params(probe: Probe, raw_http: httpx.AsyncClient) -> None:
    """缺参 / 空参 / 全空白 / NUL 字节一律 400（不是 FastAPI 的 422）。"""
    session_id, workspace = await new_session(probe, "params")
    (workspace / "pic.png").write_bytes(PNG_BYTES)

    cases = [
        {"path": "pic.png"},  # 缺 session_id
        {"session_id": session_id},  # 缺 path
        {"session_id": session_id, "path": ""},  # 空 path
        {"session_id": session_id, "path": "   "},  # 全空白
        {"session_id": "", "path": "pic.png"},  # 空 session_id
        {"session_id": session_id, "path": "a\x00b.png"},  # NUL 字节
    ]
    for params in cases:
        response = await raw_http.get(ENDPOINT, params=params)
        assert response.status_code == 400, (
            params,
            response.status_code,
            response.text,
        )
        assert response.json()["error"] == "bad_request", (params, response.text)


# ── 403：越界 ────────────────────────────────────────────────────


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_refuses_escapes_and_cross_session(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """`..` 穿越 / 符号链接逃逸 / 跨会话隔离一律 403，且拿不到文件内容。"""
    alpha_id, alpha = await new_session(probe, "alpha")
    beta_id, beta = await new_session(probe, "beta")

    secret = probe.env.root / "outside-secret.png"
    secret.write_bytes(b"OUTSIDE-SECRET-BYTES")
    (alpha / "escape.png").symlink_to(secret)
    (beta / "beta-secret.png").write_bytes(b"BETA-SECRET-BYTES")

    cases = [
        "../outside-secret.png",
        "../../outside-secret.png",
        "sub/../../outside-secret.png",
        str(secret),  # 绝对路径但不在 workspace 内
        str(beta / "beta-secret.png"),  # 另一会话的 workspace（跨会话隔离）
        "escape.png",  # 符号链接指向 workspace 外
        "../",  # workspace 根本身（归一后落在根之外）
    ]
    for path in cases:
        response = await image(raw_http, alpha_id, path)
        assert response.status_code == 403, (path, response.status_code, response.text)
        assert response.json()["error"] == "forbidden", (path, response.text)
        assert b"SECRET" not in response.content, (path, "越界内容泄露")

    # URL 编码形态同样 403（服务端只对**真的到达**的编码形态负责；客户端自行
    # 归一的 `..` 压根不会以穿越形态上线——这里直接构造原始 URL 绕过 httpx 归一）。
    encoded = (
        f"{ENDPOINT}?session_id={alpha_id}&path=%2e%2e%2foutside-secret.png"
        f"&path2=" + "x"
    )
    response = await raw_http.get(encoded)
    assert response.status_code == 403, (response.status_code, response.text)
    assert b"SECRET" not in response.content

    encoded_abs = f"{ENDPOINT}?session_id={alpha_id}&path=%2Fetc%2Fhosts"
    response = await raw_http.get(encoded_abs)
    assert response.status_code == 403, (response.status_code, response.text)

    # 同一 workspace 内的另一张图照常可读（403 不是"整会话拒绝"）。
    (alpha / "ok.png").write_bytes(PNG_BYTES)
    ok = await image(raw_http, alpha_id, "ok.png")
    assert ok.status_code == 200, ok.text
    # 隔离的另一侧：beta 自己的图正常。
    own = await image(raw_http, beta_id, "beta-secret.png")
    assert own.status_code == 200, own.text
    assert own.content == b"BETA-SECRET-BYTES"


# ── 404：不存在 / 非白名单 / 非会话 ──────────────────────────────


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_not_found_cases(probe: Probe, raw_http: httpx.AsyncClient) -> None:
    """会话不存在 / 扩展名不在白名单 / 文件不存在 / 目录 → 404。"""
    session_id, workspace = await new_session(probe, "notfound")
    (workspace / "notes.txt").write_bytes(b"plain text")
    (workspace / "noext").write_bytes(b"x")
    (workspace / "adir.png").mkdir()  # 名字像图片，其实是目录

    cases = [
        ("nope-session", "pic.png"),  # 会话不在内存
        (session_id, "notes.txt"),  # 白名单外扩展名
        (session_id, "noext"),  # 无扩展名
        (session_id, "missing.png"),  # 文件不存在
        (session_id, "adir.png"),  # 目录不是文件
    ]
    for sid, path in cases:
        response = await image(raw_http, sid, path)
        assert response.status_code == 404, (
            sid,
            path,
            response.status_code,
            response.text,
        )
        assert response.json()["error"] == "not_found", (path, response.text)


# ── 413：超过 10 MiB ─────────────────────────────────────────────


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_size_limit(probe: Probe, raw_http: httpx.AsyncClient) -> None:
    """恰好 10 MiB 放行；多 1 字节即 413（上限本身允许）。"""
    session_id, workspace = await new_session(probe, "size")
    limit = 10 * MIB

    exactly = workspace / "exact.png"
    exactly.write_bytes(PNG_BYTES + b"\x00" * (limit - len(PNG_BYTES)))
    assert exactly.stat().st_size == limit

    over = workspace / "over.png"
    over.write_bytes(PNG_BYTES + b"\x00" * (limit + 1 - len(PNG_BYTES)))
    assert over.stat().st_size == limit + 1

    ok = await image(raw_http, session_id, "exact.png")
    assert ok.status_code == 200, ok.text
    assert len(ok.content) == limit

    too_big = await image(raw_http, session_id, "over.png")
    assert too_big.status_code == 413, (too_big.status_code, too_big.text)
    assert too_big.json()["error"] == "payload_too_large", too_big.text


# ── 不实现 Range ─────────────────────────────────────────────────


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_range_requests_are_ignored(
    probe: Probe, raw_http: httpx.AsyncClient
) -> None:
    """带 `Range` 头仍返回 200 全量（没有 206、没有 Accept-Ranges）。"""
    session_id, workspace = await new_session(probe, "range")
    payload = PNG_BYTES + b"trailing-bytes"
    (workspace / "pic.png").write_bytes(payload)

    response = await image(
        raw_http, session_id, "pic.png", headers={"Range": "bytes=0-3"}
    )
    assert response.status_code == 200, response.status_code
    assert response.content == payload
    assert "content-range" not in response.headers, dict(response.headers)
    assert "accept-ranges" not in response.headers, dict(response.headers)
