"""媒体读取端点（``GET /api/session/media``）单元测试：字节 / 响应头 / 404 面 / 不水合。

各层各测自己那一层的契约：

- ``SessionManager.read_media``：纯解析（**不水合**）+ ``store.read_media`` 透传——
  合法 id 但对象不在返回 None、非法 id 抛 ``ValueError``（路径穿越闸门在 store 的
  路径拼接之前）、幽灵会话 LookupError、归档会话 SessionArchivedError；
- 路由：原始字节响应 + 三个缓存头 + ``nosniff``；非法 / 不存在同价 404；
  归档 404 + ``session_archived``；老池对象经端点可读并物化；memory 后端天然可读。

鉴权面（``?api_key=`` 回退）测在 ``test_gateway_http.py``（与 headers 优先级同处）。
整机链路由 probe 场景 ``libs/wing-probe/scenarios/test_session_media_read.py`` 钉住。
"""

from __future__ import annotations

import hashlib
import struct
import zlib
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from wing.config import AuthConfig, get_config
from wing.runtime import WingRuntime
from wing.schema import Message

IMMUTABLE_CACHE = "private, max-age=31536000, immutable"


# ── 图片与路径助手（本文件自足） ────────────────────────────────


def png(width: int = 2, height: int = 2, *, tint: int = 0xE0) -> bytes:
    """合法的最小 PNG（真 IHDR/IDAT/IEND 与 CRC）。"""

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


def gif(width: int = 2, height: int = 2) -> bytes:
    return b"GIF89a" + struct.pack("<HH", width, height) + b"\x00\x00\x00"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@pytest.fixture
def runtime(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> WingRuntime:
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    return WingRuntime()


def sessions_root() -> Path:
    return get_config().sessions.resolved_path()


def media_dir(session_id: str) -> Path:
    return sessions_root() / session_id / "media"


def _seed_session(runtime: WingRuntime, *, backend: str | None = None) -> str:
    session = runtime.create_session(backend=backend)
    session.context_manager.add_message(Message(role="user", content="hi"))
    return session.session_id


# ── 1. manager 层 ──────────────────────────────────────────────


class TestManagerReadMedia:
    @pytest.mark.asyncio
    async def test_reads_uploaded_bytes(self, runtime: WingRuntime) -> None:
        sid = _seed_session(runtime)
        data = png(3, 2, tint=0x22)
        ref = runtime.upload_media(sid, data, name="shot.png")

        assert runtime.read_media(sid, ref.id) == data

    @pytest.mark.asyncio
    async def test_missing_object_is_none(self, runtime: WingRuntime) -> None:
        sid = _seed_session(runtime)
        assert runtime.read_media(sid, "0" * 64) is None

    @pytest.mark.asyncio
    async def test_invalid_media_id_raises_value_error(
        self, runtime: WingRuntime
    ) -> None:
        sid = _seed_session(runtime)
        for bad in ("../etc/passwd", "ABCDEF", "0" * 63, "0" * 65, "z" * 64):
            with pytest.raises(ValueError):
                runtime.read_media(sid, bad)

    @pytest.mark.asyncio
    async def test_ghost_session_is_lookup_error(self, runtime: WingRuntime) -> None:
        with pytest.raises(LookupError):
            runtime.read_media("ghost-read-1", "0" * 64)

    @pytest.mark.asyncio
    async def test_read_does_not_hydrate_an_evicted_session(
        self, runtime: WingRuntime
    ) -> None:
        sid = _seed_session(runtime)
        data = png(2, 2, tint=0x33)
        ref = runtime.upload_media(sid, data)
        assert runtime.sm.evict(sid, reason="test") is True
        await runtime.sm.wait_teardowns()
        assert runtime.sm.get_session(sid) is None

        assert runtime.read_media(sid, ref.id) == data
        assert runtime.sm.get_session(sid) is None, "读一张图把已逐出的会话唤醒了"

    @pytest.mark.asyncio
    async def test_memory_backend_reads_its_bucket(self, runtime: WingRuntime) -> None:
        """memory 后端天然可读（字节就在进程内桶里）——不是 404 面。"""
        session = runtime.create_session(backend="memory")
        data = png(2, 2, tint=0x44)
        ref = runtime.upload_media(session.session_id, data)

        assert runtime.read_media(session.session_id, ref.id) == data
        assert runtime.read_media(session.session_id, "0" * 64) is None

    @pytest.mark.asyncio
    async def test_legacy_pool_object_is_readable_and_materialized(
        self, runtime: WingRuntime
    ) -> None:
        """老共享池里的对象经读路径回退命中并物化进会话目录（既有存储契约）。"""
        sid = _seed_session(runtime)
        data = png(2, 2, tint=0x55)
        object_id = sha256(data)
        legacy = sessions_root() / ".media" / object_id[:2] / object_id
        legacy.parent.mkdir(parents=True, exist_ok=True)
        legacy.write_bytes(data)

        assert runtime.read_media(sid, object_id) == data
        assert (media_dir(sid) / object_id).read_bytes() == data


# ── 2. 路由层 ──────────────────────────────────────────────────


def _gateway_config() -> MagicMock:
    config = MagicMock()
    config.gateway.auth = AuthConfig(enabled=False, keys=[])
    return config


@pytest.fixture
def real_client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """真实 WingRuntime + 独立 sessions 根 + TestClient（不起 uvicorn）。"""
    from wing.gateway.server import GatewayServer

    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    with patch("wing.gateway.server.load_config") as mock_load_config:
        mock_load_config.return_value = _gateway_config()
        server = GatewayServer()
    server.runtime = WingRuntime()
    with TestClient(server._app) as client:
        yield client, server.runtime


class TestReadMediaRoute:
    def _create_session(
        self, client: TestClient, runtime: WingRuntime, *, backend: str | None = None
    ) -> str:
        body: dict[str, Any] = {} if backend is None else {"backend": backend}
        response = client.post("/api/session/create", json=body)
        assert response.status_code == 200, response.text
        sid = str(response.json()["session_id"])
        session = runtime.get_session(sid)
        assert session is not None
        session.context_manager.add_message(Message(role="user", content="hi"))
        return sid

    def _upload(self, client: TestClient, sid: str, data: bytes) -> str:
        response = client.post(
            "/api/session/media",
            params={"session_id": sid, "name": "shot.png"},
            content=data,
        )
        assert response.status_code == 200, response.text
        return str(response.json()["media"]["id"])

    def test_reads_bytes_with_cache_headers(self, real_client) -> None:
        client, runtime = real_client
        sid = self._create_session(client, runtime)
        data = png(6, 5, tint=0xAA)
        media_id = self._upload(client, sid, data)

        response = client.get(
            "/api/session/media",
            params={"session_id": sid, "media_id": media_id},
        )

        assert response.status_code == 200, response.text
        assert response.content == data
        assert response.headers["content-type"] == "image/png"
        assert response.headers["etag"] == f'"{media_id}"'
        assert response.headers["cache-control"] == IMMUTABLE_CACHE
        assert response.headers["x-content-type-options"] == "nosniff"

    def test_content_type_follows_the_bytes_not_the_name(self, real_client) -> None:
        """mime 由 magic bytes 现读（不水合会话就拿不到 MediaRef 的 mime）。"""
        client, runtime = real_client
        sid = self._create_session(client, runtime)
        data = gif(2, 2)
        media_id = self._upload(client, sid, data)

        response = client.get(
            "/api/session/media",
            params={"session_id": sid, "media_id": media_id},
        )

        assert response.status_code == 200, response.text
        assert response.headers["content-type"] == "image/gif"

    def test_unknown_media_id_is_404(self, real_client) -> None:
        client, runtime = real_client
        sid = self._create_session(client, runtime)

        response = client.get(
            "/api/session/media", params={"session_id": sid, "media_id": "0" * 64}
        )

        assert response.status_code == 404, response.text
        assert response.json() == {"error": "not_found", "detail": "media not found"}
        assert not media_dir(sid).exists()

    @pytest.mark.parametrize(
        "media_id",
        ["../etc/passwd", "a" * 64, "0" * 63, "0" * 65, "0" * 62 + "zz"],
        ids=["traversal", "uppercase-hex", "too-short", "too-long", "not-hex"],
    )
    def test_invalid_media_id_is_404_like_missing(self, real_client, media_id) -> None:
        """非法 id 与"不存在"同价（闸门先于任何文件系统操作）。"""
        client, runtime = real_client
        sid = self._create_session(client, runtime)

        response = client.get(
            "/api/session/media", params={"session_id": sid, "media_id": media_id}
        )

        assert response.status_code == 404, response.text
        assert response.json()["detail"] == "media not found", response.text
        assert not (sessions_root().parent / "etc").exists()

    def test_ghost_session_is_404(self, real_client) -> None:
        client, _ = real_client
        response = client.get(
            "/api/session/media",
            params={"session_id": "nope", "media_id": "0" * 64},
        )
        assert response.status_code == 404, response.text
        assert response.json()["detail"] == "session not found"
        assert not (sessions_root() / "nope").exists()

    def test_archived_session_is_404_with_the_actionable_marker(
        self, real_client
    ) -> None:
        client, runtime = real_client
        sid = self._create_session(client, runtime)
        media_id = self._upload(client, sid, png())
        archived = client.post(
            "/api/session/archive", json={"session_id": sid, "archived": True}
        )
        assert archived.status_code == 200 and archived.json()["changed"] is True

        response = client.get(
            "/api/session/media", params={"session_id": sid, "media_id": media_id}
        )

        assert response.status_code == 404, response.text
        assert response.json()["error"] == "session_archived"
        assert "restore" in response.json()["detail"]

    def test_read_does_not_hydrate_an_evicted_session(self, real_client) -> None:
        client, runtime = real_client
        sid = self._create_session(client, runtime)
        data = png(2, 2, tint=0xBB)
        media_id = self._upload(client, sid, data)
        released = client.post("/api/session/release", json={"session_id": sid})
        assert released.status_code == 200 and released.json()["released"] is True
        assert runtime.get_session(sid) is None

        response = client.get(
            "/api/session/media", params={"session_id": sid, "media_id": media_id}
        )

        assert response.status_code == 200, response.text
        assert response.content == data
        assert runtime.get_session(sid) is None, "读端点把已逐出的会话唤醒了"

    def test_memory_backend_serves_bytes(self, real_client) -> None:
        client, runtime = real_client
        sid = self._create_session(client, runtime, backend="memory")
        data = png(2, 2, tint=0xCC)
        media_id = self._upload(client, sid, data)

        response = client.get(
            "/api/session/media", params={"session_id": sid, "media_id": media_id}
        )

        assert response.status_code == 200, response.text
        assert response.content == data

    def test_missing_query_params_are_422(self, real_client) -> None:
        client, _ = real_client
        assert client.get("/api/session/media").status_code == 422
        assert (
            client.get("/api/session/media", params={"session_id": "x"}).status_code
            == 422
        )
