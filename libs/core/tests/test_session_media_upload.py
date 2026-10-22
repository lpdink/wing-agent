"""上传端点（``POST /api/session/media``）单元测试：字节闸门 / 会话解析 / 路由映射。

各层各测自己那一层的契约：

- ``wing.media.upload_image_ref``：**纯闸门**——五道检查（非空 / 上限 / 展示名长度 /
  格式 / 尺寸）的顺序与文案、id = sha256、name 归一、返回的事实；
- ``MediaRef``：展示名的模型层上限（``max_length``，pydantic 兜底）；
- ``SessionManager.upload_media``：会话解析（**不水合**）+ 落盘——幂等、拒绝零副作用、
  幽灵会话不建目录、memory 后端、已逐出会话不因上传被唤醒；``session_exists`` 同判据、
  同样不水合；
- 路由：query / 原始字节体 → 200/400/404/422 映射与响应形状（真实 ``WingRuntime`` +
  ``TestClient``——mock 掉 runtime 就只剩路由胶水，证明不了"字节真的进了会话目录"），
  以及**收包顺序**（会话存在性 → ``Content-Length`` 预检 → 收包，用 ASGI 读包计数器取证）。

整机链路（上传 → 发消息携带 ref → 假 Provider 请求体 image part 逐字节 == 上传字节）与
"只写请求头就拿到拒绝"的收包顺序由 probe 场景
``libs/wing-probe/scenarios/test_session_media_upload.py`` 钉住。
"""

from __future__ import annotations

import hashlib
import struct
import zlib
from pathlib import Path

import pytest
from fastapi.testclient import TestClient
from pydantic import ValidationError

from _support import BodyWatcher, gateway_server, media_dir, sessions_root
from wing.common.utils import InvalidInputError
from wing.config import get_config
from wing.media import upload_image_ref
from wing.runtime import WingRuntime
from wing.schema import MEDIA_NAME_MAX_LENGTH, MediaRef, Message


# ── 图片工厂（本文件自足：stdlib 造合法头部，不解码像素） ──────────


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


def _jpeg_segment(marker: int, payload: bytes) -> bytes:
    return bytes([0xFF, marker]) + struct.pack(">H", len(payload) + 2) + payload


def jpeg(width: int, height: int) -> bytes:
    """SOI + APP0 + SOF0（解析必须跳过 APP0 找到帧头）。"""
    sof = b"\x08" + struct.pack(">HH", height, width) + b"\x03\x01\x22\x00\x02\x11\x01"
    return (
        b"\xff\xd8"
        + _jpeg_segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")
        + _jpeg_segment(0xC0, sof)
    )


def gif(width: int, height: int) -> bytes:
    return b"GIF89a" + struct.pack("<HH", width, height) + b"\x00\x00\x00"


def webp(width: int, height: int) -> bytes:
    payload = (
        b"\x00\x00\x00\x00"
        + struct.pack("<I", width - 1)[:3]
        + struct.pack("<I", height - 1)[:3]
    )
    riff_size = 4 + 8 + len(payload)
    return (
        b"RIFF"
        + struct.pack("<I", riff_size)
        + b"WEBP"
        + b"VP8X"
        + struct.pack("<I", len(payload))
        + payload
    )


def cgbi_png() -> bytes:
    """Apple CgBI 变体 PNG：签名 + ``CgBI`` chunk（IHDR 被推到偏移 28）。

    文件本身合法可显示，只是尺寸无法按标准 PNG 布局解析。
    """
    return (
        b"\x89PNG\r\n\x1a\n"
        + struct.pack(">I", 4)
        + b"CgBI"
        + b"PROF"  # CgBI chunk payload（内容不参与判定）
        + b"\x00\x00\x00\x00"  # chunk CRC（内容不参与判定）
        + b"\x00" * 16
    )


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ── 1. 纯闸门：upload_image_ref ────────────────────────────────


class TestUploadImageRef:
    """字节闸门的事实与拒绝路径（零存储副作用是纯函数的天然属性）。"""

    @pytest.mark.parametrize(
        ("data", "mime", "width", "height"),
        [
            (png(3, 2), "image/png", 3, 2),
            (jpeg(5, 4), "image/jpeg", 5, 4),
            (gif(7, 6), "image/gif", 7, 6),
            (webp(9, 8), "image/webp", 9, 8),
        ],
    )
    def test_facts_for_supported_formats(
        self, data: bytes, mime: str, width: int, height: int
    ) -> None:
        ref = upload_image_ref(data, max_bytes=1 << 20, name="shot.png")
        assert ref.id == sha256(data)
        assert ref.mime == mime
        assert ref.bytes == len(data)
        assert (ref.width, ref.height) == (width, height)
        assert ref.name == "shot.png"

    def test_name_defaults_to_none_and_empty_string_is_none(self) -> None:
        data = png()
        assert upload_image_ref(data, max_bytes=1 << 20).name is None
        assert upload_image_ref(data, max_bytes=1 << 20, name="").name is None

    def test_empty_body_is_rejected(self) -> None:
        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(b"", max_bytes=1 << 20)
        assert "empty request body" in str(failure.value)

    def test_oversize_is_rejected_with_sizes(self) -> None:
        data = png()
        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(data, max_bytes=len(data) - 1)
        message = str(failure.value)
        assert "image too large" in message
        assert "images.max_bytes" in message

    def test_exact_limit_is_accepted(self) -> None:
        """上限是"不超过"：恰好等于 max_bytes 合法（严格大于才拒）。"""
        data = png()
        ref = upload_image_ref(data, max_bytes=len(data))
        assert ref.bytes == len(data)

    def test_unsupported_format_lists_supported_ones(self) -> None:
        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(b"%PDF-1.4 not an image", max_bytes=1 << 20)
        message = str(failure.value)
        assert "not a supported image" in message
        assert "gif/jpeg/png/webp" in message

    def test_truncated_header_is_rejected_by_its_own_reason(self) -> None:
        """magic bytes 认得、头部截断：报"尺寸不可解析"而不是"格式不支持"。"""
        truncated = png()[:20]
        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(truncated, max_bytes=1 << 20)
        assert "cannot read png image dimensions" in str(failure.value)

    def test_apple_cgbi_png_is_named_as_a_variant_not_as_corruption(self) -> None:
        """CgBI（Apple 非标准 PNG）：归因是"变体需转换"，不是"头部损坏"。

        与读图路径同一条归因（`read_image.py` 的 CgBI 分支）——同一个文件不该因
        入口不同得到两种可诊断性。
        """
        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(cgbi_png(), max_bytes=1 << 20)
        message = str(failure.value)
        assert "Apple CgBI PNG variant" in message
        assert "Convert it first" in message
        assert "truncated or corrupt" not in message

    def test_size_check_precedes_format_check(self) -> None:
        """顺序：超限的垃圾字节报"过大"（先 O(1) 检查，不为非法载荷做解析）。"""
        garbage = b"x" * 100
        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(garbage, max_bytes=10)
        assert "image too large" in str(failure.value)

        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(b"", max_bytes=0)
        assert "empty request body" in str(failure.value)


# ── 1b. 展示名的长度上限（客户端可控字符串） ────────────────────


class TestMediaNameCap:
    """``MediaRef.name`` 的长度上限：客户端可控字符串进响应、进 history、进界面。

    两个消费面各测一次：模型字段（pydantic 兜底）与上传闸门（把超限翻译成 400 的
    ``InvalidInputError``，而不是让 ``ValidationError`` 冒到 500）。
    """

    def test_model_field_caps_the_name(self) -> None:
        def ref(name: str) -> MediaRef:
            return MediaRef(
                id="a" * 64, mime="image/png", bytes=1, width=1, height=1, name=name
            )

        assert ref("a" * MEDIA_NAME_MAX_LENGTH).name == "a" * MEDIA_NAME_MAX_LENGTH
        with pytest.raises(ValidationError):
            ref("a" * (MEDIA_NAME_MAX_LENGTH + 1))

    def test_gate_rejects_an_overlong_name(self) -> None:
        data = png()
        longest = "名" * MEDIA_NAME_MAX_LENGTH
        assert (
            upload_image_ref(data, max_bytes=1 << 20, name=longest).name == longest
        ), "长度按**字符**数算（多字节字符不提前撞线）"
        with pytest.raises(InvalidInputError) as failure:
            upload_image_ref(
                data, max_bytes=1 << 20, name="名" * (MEDIA_NAME_MAX_LENGTH + 1)
            )
        message = str(failure.value)
        assert "name too long" in message and str(MEDIA_NAME_MAX_LENGTH) in message


# ── 2. 会话层用例：SessionManager.upload_media ─────────────────


class TestManagerUpload:
    """会话解析（不水合）+ 落盘：幂等 / 零副作用 / 后端差异。"""

    @pytest.mark.asyncio
    async def test_upload_writes_object_and_returns_ref(
        self, isolated_runtime: WingRuntime
    ) -> None:
        session = isolated_runtime.create_session()
        session.context_manager.add_message(Message(role="user", content="hi"))
        data = png(4, 3, tint=0x11)

        ref = isolated_runtime.upload_media(session.session_id, data, name="pasted.png")

        assert ref.id == sha256(data) and ref.bytes == len(data)
        assert (ref.width, ref.height) == (4, 3)
        object_path = media_dir(session.session_id) / ref.id
        assert object_path.read_bytes() == data
        assert (
            isolated_runtime.sm._stores["file"].read_media(session.session_id, ref.id)
            == data
        )

    @pytest.mark.asyncio
    async def test_second_upload_of_same_bytes_returns_same_id_without_rewrite(
        self, isolated_runtime: WingRuntime
    ) -> None:
        session = isolated_runtime.create_session()
        session.context_manager.add_message(Message(role="user", content="hi"))
        data = png(2, 2, tint=0x22)

        first = isolated_runtime.upload_media(session.session_id, data, name="one.png")
        before = (media_dir(session.session_id) / first.id).stat()

        second = isolated_runtime.upload_media(session.session_id, data, name="two.png")

        assert second.id == first.id
        # name 是"本次调用的展示素材"，不随对象去重：各自回显各自的请求值。
        assert (first.name, second.name) == ("one.png", "two.png")
        after = (media_dir(session.session_id) / first.id).stat()
        assert (after.st_ino, after.st_mtime_ns, after.st_size) == (
            before.st_ino,
            before.st_mtime_ns,
            before.st_size,
        ), "同字节二次上传改写了对象文件（应幂等跳过）"
        assert [p.name for p in media_dir(session.session_id).iterdir()] == [first.id]

    @pytest.mark.asyncio
    async def test_rejected_uploads_leave_no_trace(
        self, isolated_runtime: WingRuntime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        session = isolated_runtime.create_session()
        session.context_manager.add_message(Message(role="user", content="hi"))
        sid = session.session_id
        good = png()
        # 上限压到 good 之下：合法 PNG 也进不了闸门 → 四条拒绝路径都走一遍。
        monkeypatch.setattr(get_config().images, "max_bytes", len(good) - 1)

        for bad in (b"", b"not an image at all", good, good[:20]):
            with pytest.raises(InvalidInputError):
                isolated_runtime.upload_media(sid, bad)
            assert not media_dir(sid).exists(), "拒绝路径在存储上留下了痕迹"

        # 闸门没有把会话 / 存储搞坏：放宽上限后合法字节照常入库。
        monkeypatch.setattr(get_config().images, "max_bytes", len(good))
        ref = isolated_runtime.upload_media(sid, good)
        assert (media_dir(sid) / ref.id).read_bytes() == good

    @pytest.mark.asyncio
    async def test_unknown_session_is_lookup_error_and_creates_nothing(
        self, isolated_runtime: WingRuntime
    ) -> None:
        with pytest.raises(LookupError):
            isolated_runtime.upload_media("ghost-session", png())
        assert not (sessions_root() / "ghost-session").exists()

        # 非法 id（路径穿越）同价：按"不存在"处理，绝不触达 store。
        with pytest.raises(LookupError):
            isolated_runtime.upload_media("../escape", png())
        assert not (sessions_root().parent / "escape").exists()

    @pytest.mark.asyncio
    async def test_evicted_session_is_not_hydrated(
        self, isolated_runtime: WingRuntime
    ) -> None:
        """上传不水合：被逐出的会话保持 inactive，对象照常落盘。"""
        session = isolated_runtime.create_session()
        session.context_manager.add_message(Message(role="user", content="hi"))
        sid = session.session_id
        assert isolated_runtime.sm.evict(sid, reason="test") is True
        await isolated_runtime.sm.wait_teardowns()
        assert isolated_runtime.sm.get_session(sid) is None

        ref = isolated_runtime.upload_media(sid, png(2, 2, tint=0x33), name="late.png")

        assert isolated_runtime.sm.get_session(sid) is None, "上传把已逐出的会话唤醒了"
        assert (media_dir(sid) / ref.id).read_bytes() == png(2, 2, tint=0x33)

    @pytest.mark.asyncio
    async def test_alias_id_writes_into_the_canonical_session_dir(
        self, isolated_runtime: WingRuntime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """别名（大小写 / 归一化不敏感 FS）解析成磁盘真名：落真名目录、不另起一份。

        大小写敏感的 FS 上真别名不存在，这里照 ``test_session_create_or_adopt.py``
        的做法**造一个**——要钉的是"写目标是解析出的键，不是请求字符串"。
        """
        session = isolated_runtime.create_session()
        session.context_manager.add_message(Message(role="user", content="hi"))
        sid = session.session_id
        store = isolated_runtime.sm._stores["file"]
        original = store.resolve_stored_id

        def alias_resolve(session_id: str) -> str | None:
            return sid if session_id == "alias-of-session" else original(session_id)

        monkeypatch.setattr(store, "resolve_stored_id", alias_resolve)
        data = png(2, 2, tint=0x88)

        ref = isolated_runtime.upload_media("alias-of-session", data)

        assert (media_dir(sid) / ref.id).read_bytes() == data
        assert not (sessions_root() / "alias-of-session").exists()

    @pytest.mark.asyncio
    async def test_memory_backend_keeps_object_in_session_bucket(
        self, isolated_runtime: WingRuntime
    ) -> None:
        session = isolated_runtime.create_session(backend="memory")
        data = png(3, 3, tint=0x44)

        ref = isolated_runtime.upload_media(session.session_id, data)

        store = isolated_runtime.sm._stores["memory"]
        assert store.read_media(session.session_id, ref.id) == data
        assert store.media_path(session.session_id, ref.id) is None


# ── 2b. 会话解析：SessionManager.session_exists ────────────────


class TestSessionExists:
    """纯解析（不水合）：判据与 ``upload_media`` 是同一个 ``_resolve_with_store``。"""

    @pytest.mark.asyncio
    async def test_exists_parses_without_hydrating(
        self, isolated_runtime: WingRuntime
    ) -> None:
        session = isolated_runtime.create_session()
        session.context_manager.add_message(Message(role="user", content="hi"))
        sid = session.session_id
        assert isolated_runtime.session_exists(sid) is True

        assert isolated_runtime.sm.evict(sid, reason="test") is True
        await isolated_runtime.sm.wait_teardowns()
        assert isolated_runtime.sm.get_session(sid) is None
        assert isolated_runtime.session_exists(sid) is True, "被逐出的会话仍在磁盘上"
        assert isolated_runtime.sm.get_session(sid) is None, "解析不该把它水合回来"

        assert isolated_runtime.session_exists("ghost-session") is False
        # 非法 id（路径穿越）与"不存在"同价：绝不触达 store。
        assert isolated_runtime.session_exists("../escape") is False


# ── 3. 路由：POST /api/session/media ──────────────────────────


@pytest.fixture
def real_client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """真实 WingRuntime + 独立 sessions 根 + TestClient（不起 uvicorn）。"""
    with gateway_server(tmp_path, monkeypatch) as server:
        with TestClient(server._app) as client:
            yield client, sessions_root()


@pytest.fixture
def body_watching_client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """同 ``real_client``，但外层包了读包计数器（见 ``BodyWatcher``）。"""
    with gateway_server(tmp_path, monkeypatch) as server:
        watcher = BodyWatcher(server._app)
        with TestClient(watcher) as client:
            yield client, sessions_root(), watcher


class TestUploadRoute:
    """HTTP 面：原始字节体 + query 参数 → 响应形状与错误映射。"""

    def _create_session(self, client: TestClient) -> str:
        response = client.post("/api/session/create", json={})
        assert response.status_code == 200, response.text
        return str(response.json()["session_id"])

    def test_upload_returns_ref_and_persists_bytes(self, real_client) -> None:
        client, root = real_client
        sid = self._create_session(client)
        data = png(6, 5, tint=0x55)

        response = client.post(
            "/api/session/media",
            params={"session_id": sid, "name": "shot.png"},
            content=data,
            # 原始字节体：客户端给的 content-type 是什么都不影响判读（按内容判定）。
            headers={"content-type": "application/octet-stream"},
        )

        assert response.status_code == 200, response.text
        body = response.json()
        assert body["ok"] is True
        assert body["media"] == {
            "id": sha256(data),
            "mime": "image/png",
            "bytes": len(data),
            "width": 6,
            "height": 5,
            "name": "shot.png",
        }, body
        assert (root / sid / "media" / sha256(data)).read_bytes() == data

    def test_name_is_optional(self, real_client) -> None:
        client, _ = real_client
        sid = self._create_session(client)
        data = png(2, 2, tint=0x66)

        omitted = client.post(
            "/api/session/media", params={"session_id": sid}, content=data
        )
        assert omitted.status_code == 200, omitted.text
        assert omitted.json()["media"]["name"] is None

        empty = client.post(
            "/api/session/media",
            params={"session_id": sid, "name": ""},
            content=data,
        )
        assert empty.status_code == 200, empty.text
        assert empty.json()["media"]["name"] is None

    @pytest.mark.parametrize(
        "data",
        [b"", b"not an image at all", png()[:20]],
        ids=["empty", "garbage", "truncated"],
    )
    def test_invalid_bytes_are_400_without_media_dir(self, real_client, data) -> None:
        client, root = real_client
        sid = self._create_session(client)

        response = client.post(
            "/api/session/media", params={"session_id": sid}, content=data
        )

        assert response.status_code == 400, response.text
        assert response.json()["error"] == "bad_request"
        assert response.json()["detail"], response.text
        assert not (root / sid / "media").exists(), "400 路径在存储上留下了痕迹"

    def test_cgbi_variant_is_400_with_the_variant_message(self, real_client) -> None:
        """HTTP 面同样按"变体需转换"归因，且不留痕。"""
        client, root = real_client
        sid = self._create_session(client)

        response = client.post(
            "/api/session/media", params={"session_id": sid}, content=cgbi_png()
        )

        assert response.status_code == 400, response.text
        assert "Apple CgBI PNG variant" in response.json()["detail"], response.text
        assert not (root / sid / "media").exists()

    def test_oversize_bytes_are_400_against_config_limit(
        self, real_client, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        client, root = real_client
        sid = self._create_session(client)
        data = png(2, 2, tint=0x77)
        monkeypatch.setattr(get_config().images, "max_bytes", len(data) - 1)

        response = client.post(
            "/api/session/media", params={"session_id": sid}, content=data
        )

        assert response.status_code == 400, response.text
        assert "images.max_bytes" in response.json()["detail"]
        assert not (root / sid / "media").exists()

    def test_unknown_session_is_404(self, real_client) -> None:
        client, root = real_client
        # 会话存在性先于内容校验（与 send 同序）：body 合法或非法都回 404。
        for data in (png(), b"not an image at all"):
            response = client.post(
                "/api/session/media", params={"session_id": "nope"}, content=data
            )
            assert response.status_code == 404, response.text
            assert response.json() == {
                "error": "not_found",
                "detail": "session not found",
            }
        assert not (root / "nope").exists()

    def test_overlong_name_is_400_and_never_reaches_storage(self, real_client) -> None:
        """展示名超限 → 400（与字节闸门同一句理由）；恰好到上限的名字照常回显。"""
        client, root = real_client
        sid = self._create_session(client)
        data = png(2, 2, tint=0x99)

        response = client.post(
            "/api/session/media",
            params={"session_id": sid, "name": "n" * (MEDIA_NAME_MAX_LENGTH + 1)},
            content=data,
        )
        assert response.status_code == 400, response.text
        assert "name too long" in response.json()["detail"], response.text
        assert not (root / sid / "media").exists(), "400 路径在存储上留下了痕迹"

        longest = "名" * MEDIA_NAME_MAX_LENGTH
        accepted = client.post(
            "/api/session/media",
            params={"session_id": sid, "name": longest},
            content=data,
        )
        assert accepted.status_code == 200, accepted.text
        assert accepted.json()["media"]["name"] == longest

    def test_missing_session_id_is_422(self, real_client) -> None:
        client, _ = real_client
        response = client.post("/api/session/media", content=png())
        assert response.status_code == 422, response.text

    def test_ghost_session_is_404_before_the_body_is_read(
        self, body_watching_client
    ) -> None:
        """幽灵会话的 404 不以"读完整个 body"为代价（Starlette 的 ``body()`` 无上限）。"""
        client, root, watcher = body_watching_client
        response = client.post(
            "/api/session/media", params={"session_id": "nope"}, content=b"x" * 4096
        )
        assert response.status_code == 404, response.text
        assert response.json()["detail"] == "session not found"
        assert watcher.request_bodies() == [], "404 路径把请求体读走了"
        assert not (root / "nope").exists()

    def test_declared_oversize_is_rejected_before_the_body_is_read(
        self, body_watching_client, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``Content-Length`` 预检：声明超限直接 400，一个字节都不读。"""
        client, root, watcher = body_watching_client
        sid = self._create_session(client)
        watcher.messages.clear()  # 建会话那次读了自己的 body，与本次无关
        monkeypatch.setattr(get_config().images, "max_bytes", 1024)

        response = client.post(
            "/api/session/media",
            params={"session_id": sid},
            content=b"x" * 4096,
        )

        assert response.status_code == 400, response.text
        detail = response.json()["detail"]
        # 与字节闸门同一句文案（同一个拒绝不该有两套说法）。
        assert "image too large" in detail and "images.max_bytes" in detail, detail
        assert watcher.request_bodies() == [], "声明超限的请求体被读进来了"
        assert not (root / sid / "media").exists()

    def test_oversize_body_without_content_length_is_still_rejected_by_the_gate(
        self, real_client, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """没有 ``Content-Length``（分块传输 / 声明缺失）时由字节闸门按真实字节兜底。

        生成器 body ⇒ httpx 走 chunked（无 CL 头），预检无从下手——闸门是唯一的闸门。
        """
        client, root = real_client
        sid = self._create_session(client)
        monkeypatch.setattr(get_config().images, "max_bytes", 1024)

        def chunks():
            yield b"x" * 2048
            yield b"y" * 2048

        response = client.post(
            "/api/session/media", params={"session_id": sid}, content=chunks()
        )

        assert response.status_code == 400, response.text
        assert "images.max_bytes" in response.json()["detail"], response.text
        assert not (root / sid / "media").exists()
