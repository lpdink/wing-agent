"""媒体读取端点（``GET /api/session/media``）probe 场景。

被守的语义（design.md §5 B1 / §4.4 的消费语境）：

- **成功四件套**：上传 → 按 id 读回**原始字节**（非 JSON）：body 逐字节相等 +
  ``Content-Type``（由 magic bytes 现读）+ ``ETag: "<id>"`` + ``Cache-Control: private,
  max-age=31536000, immutable`` + ``X-Content-Type-Options: nosniff``；
- **404 面**：不存在的 id（合法 64-hex）/ 非法 id（非 hex / 路径穿越）/ 幽灵会话
  一律 404（前两者 ``media not found``、末者 ``session not found``），且不产生任何
  存储痕迹；
- **归档冻结**：归档会话的读取命中 ``SessionArchivedError`` —— 404 +
  结构化 ``error="session_archived"``（可行动提示，与"真不存在"区分）；
- **不水合**：被逐出的会话读自己的图照常 200，且读完之后**仍不在内存**
  （``/api/session/list`` 里仍是 ``inactive``）——读图不该把冷会话翻成活跃；
- **老池回退 + 物化**：只有 ``.media/<id[:2]>/<id>`` 有对象时照常可读，且读路径把
  它幂等物化进 ``<sid>/media/``（既有存储契约）；
- **memory 后端天然可读**：字节在进程内桶里 → 200 + 字节相等（不是 404 面）；
- **鉴权开启时 `?api_key=` 可读**：``<img>`` 标签带不了请求头，query key 是这条
  通道的存在前提（与 WS 既有模式一致）；无 key / 坏 key 仍 401。

断言锚定三个取证面：**HTTP 响应**（状态码 / 头 / 字节，留档在 ``http.calls``）、
**文件系统**（对象落点与物化）、**列表投影**（inactive 痕迹）。图片字节由场景现场
生成（stdlib zlib + struct 构造的合法 PNG），独立成文件、不 import 其它场景模块。
"""

from __future__ import annotations

import hashlib
import struct
import zlib
from pathlib import Path
from typing import Any

import pytest

from wing_probe import DriverHttp, DriverHttpError, Probe, Turn

# ── 模型 / 环境旋钮（场景之间零共享） ────────────────────────────

MEDIA_MODEL = "probe/media-read"

MEDIA_SPEC: dict[str, Any] = {
    "name": MEDIA_MODEL,
    "display_name": "Media Read",
    "description": "declares vision capability",
    "capabilities": {"vision": True},
}

#: 鉴权场景的 key（driver 用它连接；query key 断言用独立的无头客户端）。
ADMIN_KEY = "probe-media-key"

IMMUTABLE_CACHE = "private, max-age=31536000, immutable"

# ── 助手（本文件自足） ──────────────────────────────────────────


def png(width: int = 2, height: int = 2, *, tint: int = 0xE0) -> bytes:
    """合法的最小 PNG（真 IHDR/IDAT/IEND 与 CRC）；``tint`` 让场景互异。"""

    def chunk(tag: bytes, payload: bytes) -> bytes:
        body = tag + payload
        return (
            struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))
        )

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    pixels = bytes((tint, 0x40, 0x80))
    raw = b"".join(b"\x00" + pixels * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 1))
        + chunk(b"IEND", b"")
    )


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def media_dir(probe: Probe, session_id: str) -> Path:
    """会话私有媒体目录（未写过对象时**不存在**）。"""
    return probe.env.session_dir(session_id) / "media"


async def seed(probe: Probe) -> Any:
    """一个"是会话"的已订阅会话（有历史记录才算真的存在）。"""
    probe.register(MEDIA_MODEL, Turn.of(text="seeded"))
    session = await probe.session(model=MEDIA_MODEL)
    await session.chat("seed the history")
    return session


async def read_media(probe: Probe, session_id: str, media_id: str) -> tuple[Any, bytes]:
    """经端点读字节（原始响应）：成功返回 ``(HttpCall, bytes)``，失败抛 DriverHttpError。"""
    return await probe.driver_required.http.request_raw(
        "GET",
        "/api/session/media",
        params={"session_id": session_id, "media_id": media_id},
    )


async def live_status(probe: Probe, session_id: str) -> str:
    """``/api/session/list`` 里该会话的 status（不可见 = ``missing``）。"""
    payload = await probe.driver_required.http.request("GET", "/api/session/list")
    for row in payload["sessions"]:
        if row["id"] == session_id:
            return str(row["status"])
    return "missing"


# ── 场景 1：成功四件套（字节 / 头） ──────────────────────────────


@pytest.mark.probe_env(models=[MEDIA_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_read_returns_bytes_and_cache_headers(probe: Probe) -> None:
    """上传 → 读回：字节逐字节相等 + 三个缓存头 + nosniff + ETag == id。"""
    session = await seed(probe)
    data = png(3, 2, tint=0x44)
    http = probe.driver_required.http

    uploaded = await http.upload_media(session.session_id, data, name="shot.png")
    media_id = uploaded["media"]["id"]

    call, body = await read_media(probe, session.session_id, media_id)

    assert call.status == 200, call.render()
    assert body == data, "读回的字节与上传的字节不相等"
    assert call.response_headers["content-type"] == "image/png"
    assert call.response_headers["etag"] == f'"{media_id}"'
    assert call.response_headers["cache-control"] == IMMUTABLE_CACHE
    assert call.response_headers["x-content-type-options"] == "nosniff"

    # 读留档：走的是字节直出（非 JSON 响应），query 里只有两个参数。
    assert call.params == {"session_id": session.session_id, "media_id": media_id}, call
    session.watch.assert_never("error")


# ── 场景 2：404 面（不存在 / 非法 id / 幽灵会话） ────────────────


@pytest.mark.probe_env(models=[MEDIA_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_404_faces_leave_no_trace(probe: Probe) -> None:
    """不存在的 id / 非法 id（含路径穿越）/ 幽灵会话：全部 404 且零痕迹。"""
    session = await seed(probe)
    http = probe.driver_required.http
    data = png(2, 2, tint=0x55)
    uploaded = await http.upload_media(session.session_id, data, name="shot.png")

    # 合法 64-hex 但对象不存在。
    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "GET",
            "/api/session/media",
            params={"session_id": session.session_id, "media_id": "0" * 64},
        )
    assert failure.value.status == 404, failure.value.call.render()
    assert failure.value.call.response["detail"] == "media not found"

    # 非法 id（非 hex / 路径穿越）与"不存在"同价；闸门先于任何文件系统操作。
    for bad in ("../etc/passwd", "ABCDEF", "0" * 63, "z" * 64):
        with pytest.raises(DriverHttpError) as failure:
            await http.request(
                "GET",
                "/api/session/media",
                params={"session_id": session.session_id, "media_id": bad},
            )
        assert failure.value.status == 404, failure.value.call.render()
        assert failure.value.call.response["detail"] == "media not found", bad

    # 幽灵会话：session not found（存在性先于一切内容校验）。
    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "GET",
            "/api/session/media",
            params={"session_id": "ghost-read-1", "media_id": uploaded["media"]["id"]},
        )
    assert failure.value.status == 404, failure.value.call.render()
    assert failure.value.call.response["detail"] == "session not found"
    assert not probe.env.session_dir("ghost-read-1").exists()
    assert not (probe.env.sessions_path.parent / "etc").exists()
    # 会话媒体目录里仍只有那一个对象（404 路径没有产生第二份 / 残留）。
    assert sorted(p.name for p in media_dir(probe, session.session_id).iterdir()) == [
        uploaded["media"]["id"]
    ]


# ── 场景 3：归档冻结（404 + session_archived） ───────────────────


@pytest.mark.probe_env(models=[MEDIA_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_archived_session_read_is_frozen(probe: Probe) -> None:
    """归档后再读：404 + ``error="session_archived"``（可行动，与真不存在区分）。"""
    session = await seed(probe)
    http = probe.driver_required.http
    data = png(2, 2, tint=0x66)
    uploaded = await http.upload_media(session.session_id, data, name="shot.png")
    media_id = uploaded["media"]["id"]

    # 订阅中的会话被钉住：先退订再归档（与 test_session_archive.py 同序）。
    await http.unsubscribe(session.session_id, probe.driver_required.client_id)
    archived = await http.request(
        "POST",
        "/api/session/archive",
        body={"session_id": session.session_id, "archived": True},
    )
    assert archived["changed"] is True, archived

    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "GET",
            "/api/session/media",
            params={"session_id": session.session_id, "media_id": media_id},
        )
    assert failure.value.status == 404, failure.value.call.render()
    assert failure.value.call.response["error"] == "session_archived"
    assert "restore" in failure.value.call.response["detail"]

    # 归档是纯移动：对象跟着会话目录走（字节不变），端点只是不服务归档区。
    archived_object = (
        probe.env.sessions_path / ".archive" / session.session_id / "media" / media_id
    )
    assert archived_object.read_bytes() == data

    # 恢复（归档冻结的反面）：同一 id 立刻又能读——归档没销毁任何东西。
    restored = await http.request(
        "POST",
        "/api/session/archive",
        body={"session_id": session.session_id, "archived": False},
    )
    assert restored["changed"] is True, restored
    call, body = await read_media(probe, session.session_id, media_id)
    assert call.status == 200, call.render()
    assert body == data


# ── 场景 4：不水合（逐出后仍可读，且读不唤醒） ──────────────────


@pytest.mark.probe_env(models=[MEDIA_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_evicted_session_can_read_without_being_hydrated(probe: Probe) -> None:
    """逐出（release）后：读字节照常 200，且会话仍是 ``inactive``（不水合）。"""
    session = await seed(probe)
    http = probe.driver_required.http
    data = png(4, 4, tint=0x77)
    uploaded = await http.upload_media(session.session_id, data, name="shot.png")
    media_id = uploaded["media"]["id"]

    await http.unsubscribe(session.session_id, probe.driver_required.client_id)
    released = await http.request(
        "POST", "/api/session/release", body={"session_id": session.session_id}
    )
    assert released["released"] is True, released
    assert await live_status(probe, session.session_id) == "inactive"

    call, body = await read_media(probe, session.session_id, media_id)

    assert call.status == 200, call.render()
    assert body == data
    assert await live_status(probe, session.session_id) == "inactive", (
        "读端点把已逐出的会话水合回了内存"
    )


# ── 场景 5：老池回退 + 物化 ─────────────────────────────────────


@pytest.mark.probe_env(models=[MEDIA_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_pool_object_is_served_and_materialized(probe: Probe) -> None:
    """只有老共享池有对象时照常可读；读路径把它幂等物化进会话目录。"""
    session = await seed(probe)
    data = png(2, 2, tint=0x88)
    media_id = sha256(data)
    legacy = probe.env.sessions_path / ".media" / media_id[:2] / media_id
    legacy.parent.mkdir(parents=True, exist_ok=True)
    legacy.write_bytes(data)
    assert not media_dir(probe, session.session_id).exists()

    call, body = await read_media(probe, session.session_id, media_id)

    assert call.status == 200, call.render()
    assert body == data
    assert (media_dir(probe, session.session_id) / media_id).read_bytes() == data, (
        "老池对象没有被物化进会话目录"
    )
    # 老池只读不删（既有契约）：读完之后两份都在。
    assert legacy.read_bytes() == data


# ── 场景 6：memory 后端天然可读 ─────────────────────────────────


@pytest.mark.probe_env(models=[MEDIA_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_memory_backend_serves_bytes_from_its_bucket(probe: Probe) -> None:
    """memory 后端的字节在进程内桶里 → 天然可读（不是 404 面）。"""
    probe.register(MEDIA_MODEL, Turn.of(text="seeded"))
    session = await probe.session(model=MEDIA_MODEL, backend="memory")
    await session.chat("seed the history")
    http = probe.driver_required.http
    data = png(2, 2, tint=0x99)
    uploaded = await http.upload_media(session.session_id, data, name="shot.png")

    call, body = await read_media(probe, session.session_id, uploaded["media"]["id"])

    assert call.status == 200, call.render()
    assert body == data
    # 没有本地文件（memory 后端）：桶里没有的 id 仍如实 404。
    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "GET",
            "/api/session/media",
            params={"session_id": session.session_id, "media_id": "0" * 64},
        )
    assert failure.value.status == 404, failure.value.call.render()


# ── 场景 7：鉴权开启（`?api_key=` 是这条通道的存在前提） ─────────

#: 无头客户端（不带任何 header）——`<img>` 的等价物。
AUTH_KEYS = [{"key": ADMIN_KEY}]


@pytest.mark.probe_env(
    models=[MEDIA_SPEC], auth={"enabled": True, "keys": AUTH_KEYS}, api_key=ADMIN_KEY
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_query_api_key_serves_bytes_without_headers(probe: Probe) -> None:
    """带鉴权时：query param 里的 key 能读字节；无 key / 坏 key 仍 401。"""
    session = await seed(probe)
    data = png(2, 2, tint=0xAA)
    uploaded = await probe.driver_required.http.upload_media(
        session.session_id, data, name="shot.png"
    )
    media_id = uploaded["media"]["id"]
    endpoint = "/api/session/media"
    params = {"session_id": session.session_id, "media_id": media_id}

    keyless = DriverHttp(probe.env.gateway_url, started_at=probe.env.started_at)
    try:
        # 无 key：401（带鉴权时确实拦住了）。
        with pytest.raises(DriverHttpError) as failure:
            await keyless.request_raw("GET", endpoint, params=params)
        assert failure.value.status == 401, failure.value.call.render()

        # query key：200 + 字节相等（镜像 WS 既有模式）。
        call, body = await keyless.request_raw(
            "GET", endpoint, params={**params, "api_key": ADMIN_KEY}
        )
        assert call.status == 200, call.render()
        assert body == data

        # 坏 key 与"没有 key"同类。
        with pytest.raises(DriverHttpError) as failure:
            await keyless.request_raw(
                "GET", endpoint, params={**params, "api_key": "wrong"}
            )
        assert failure.value.status == 401, failure.value.call.render()
    finally:
        await keyless.close()
