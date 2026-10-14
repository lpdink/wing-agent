"""上传端点（``POST /api/session/media``）probe 场景（03 upload_endpoint）。

被守的语义（总任务书冻结合同 #2，服务端侧）：

- **成功四件套**：上传 → 响应 ``media`` 各字段 == 文件事实（sha256 id / mime / bytes /
  尺寸 / name）、对象落在 ``<sessions-root>/<sid>/media/<id>`` 且字节全等、目录里只有
  这一个对象、HTTP 留档显示走的是**原始字节体**通道（不带业务字段的 JSON 体）；
- **幂等**：同字节二次上传返回同一 id，对象不被重写（inode / mtime 不变、目录仍一份）；
- **拒绝零副作用**：空 body / 非图字节 / 超 ``images.max_bytes`` / 幽灵会话（404）——
  存储上不留任何痕迹（``<sid>/media/`` 不存在，或对象数不增），且闸门不会把会话弄坏
  （拒绝之后合法上传照常成功）；
- **端到端红线**：上传 → 发消息携带该 ref → 假 Provider 请求体里 user content 的
  image part **解码逐字节 == 上传字节**；`history.jsonl` 只有引用、绝无 base64；
- **name 可选**：带 → 回显；不带 → null，且对象照样可被引用（name 不是寻址键）。

断言只锚定四个取证面：**响应字段**、**文件系统**（对象落点 / 目录清单）、**假 Provider
请求留档**（请求体 image part 与字节）、**history.jsonl**（引用与无 base64）。图片字节由
场景现场生成（stdlib zlib + struct 构造的合法 PNG），独立成文件、不 import 其它场景模块。
"""

from __future__ import annotations

import base64
import hashlib
import json
import struct
import zlib
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import pytest

from wing_probe import DriverHttpError, Probe, Turn

# ── 模型 / 环境旋钮（场景之间零共享） ────────────────────────────

VISION_MODEL = "probe/upload-vision"

VISION_SPEC: dict[str, Any] = {
    "name": VISION_MODEL,
    "display_name": "Upload Vision",
    "description": "declares vision capability",
    "capabilities": {"vision": True},
}

#: 超限场景的 images 配置：上限 150 B（合法 PNG ≈ 70 B 仍可上传；补零到 200 B 即超限）。
SMALL_LIMIT_IMAGES: dict[str, Any] = {"max_bytes": 150}

#: 补零后恰好超过 SMALL_LIMIT_IMAGES 的 PNG 尺寸。
PADDED_PNG_BYTES = 200

# ── 图片与消息助手（本文件自足） ────────────────────────────────


def png(width: int = 2, height: int = 2, *, tint: int = 0xE0) -> bytes:
    """合法的最小 PNG（8-bit RGB、真 IDAT/IEND 与 CRC）；`tint` 使不同场景互异。"""

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


def png_padded(size: int, *, tint: int = 0xCC) -> bytes:
    """合法 PNG 尾部补零到恰 ``size`` 字节（IEND 之后的尾随字节不参与解析）。"""
    base = png(2, 2, tint=tint)
    assert size > len(base)
    return base + b"\x00" * (size - len(base))


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def media_dir(probe: Probe, session_id: str) -> Path:
    """会话私有媒体目录（未写过对象时**不存在**）。"""
    return probe.env.session_dir(session_id) / "media"


def media_object_path(probe: Probe, session_id: str, media_id: str) -> Path:
    """会话私有媒体对象：``<sessions-root>/<sid>/media/<id>``。"""
    return media_dir(probe, session_id) / media_id


def object_names(probe: Probe, session_id: str) -> list[str]:
    """媒体目录里的对象名清单（目录不存在 = 空清单）。"""
    directory = media_dir(probe, session_id)
    return [] if not directory.is_dir() else sorted(p.name for p in directory.iterdir())


def content_parts(message: Mapping[str, Any]) -> list[dict[str, Any]]:
    """消息 content → part 列表（str 视为单个 text part；空串 → 空表）。"""
    content = message.get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}] if content else []
    parts = content if isinstance(content, list) else []
    return [part for part in parts if isinstance(part, dict)]


def image_bytes(part: Mapping[str, Any]) -> bytes:
    """image_url data URL → 原始字节。"""
    url = str(part["image_url"]["url"])
    prefix, payload = url.split(",", 1)
    assert prefix.startswith("data:image/png;base64"), prefix
    return base64.b64decode(payload)


def user_messages(request_body: Mapping[str, Any]) -> list[dict[str, Any]]:
    messages = request_body["messages"]
    assert isinstance(messages, list), request_body
    return [m for m in messages if isinstance(m, dict) and m.get("role") == "user"]


def assert_no_base64_in_history(session_id: str, probe: Probe, data: bytes) -> None:
    """history.jsonl 只存引用、绝不落 base64（逐字面对账）。"""
    raw = (probe.env.session_dir(session_id) / "history.jsonl").read_text(
        encoding="utf-8"
    )
    assert base64.b64encode(data).decode() not in raw, "history 里落进了 base64 字节"
    assert "image_url" not in raw and "data:image" not in raw, raw[-400:]


def session_dirs(probe: Probe) -> list[str]:
    """sessions 根下的会话目录名清单（点开头的遗留池不算；不存在 = 空清单）。"""
    root = probe.env.sessions_path
    if not root.is_dir():
        return []
    return sorted(
        p.name for p in root.iterdir() if p.is_dir() and not p.name.startswith(".")
    )


def fs_is_case_insensitive(base: Path) -> bool:
    """本机文件系统对大小写是否不敏感（决定"别名"是否真的解析到同一目录）。"""
    probe_dir = base / "fs-case-probe"
    probe_dir.mkdir(parents=True, exist_ok=True)
    try:
        return (base / "FS-CASE-PROBE").is_dir()
    finally:
        probe_dir.rmdir()


# ── 场景 1：成功四件套（响应 / 存储 / 留档） ──────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_upload_returns_facts_and_persists_object(probe: Probe) -> None:
    """上传一次：响应字段 == 文件事实；对象落盘、字节全等、目录只此一份。"""
    session = await probe.session(model=VISION_MODEL)
    data = png(3, 2, tint=0x44)
    http = probe.driver_required.http

    response = await http.upload_media(session.session_id, data, name="pasted.png")

    media = response["media"]
    assert media == {
        "id": sha256(data),
        "mime": "image/png",
        "bytes": len(data),
        "width": 3,
        "height": 2,
        "name": "pasted.png",
    }, response
    assert response["ok"] is True, response

    # 存储：对象在会话私有目录里，字节逐字节相同；目录里只有它一个。
    path = media_object_path(probe, session.session_id, media["id"])
    assert path.read_bytes() == data, path
    assert object_names(probe, session.session_id) == [media["id"]]

    # 留档：走的是原始字节体通道（不是 JSON body），query 带业务参数。
    call = http.last_call(path="/api/session/media")
    assert call is not None and call.params == {
        "session_id": session.session_id,
        "name": "pasted.png",
    }, call
    assert call.body is None, "原始字节体不该被当 JSON 留档"
    assert call.detail == f"raw body: {len(data)} byte(s)", call.detail
    session.watch.assert_never("error")


# ── 场景 2：幂等（同字节二次上传） ───────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_reupload_of_same_bytes_is_idempotent(probe: Probe) -> None:
    """同字节二次上传 → 同一 id；对象不被重写（inode/mtime 不变、目录仍一份）。"""
    session = await probe.session(model=VISION_MODEL)
    data = png(2, 2, tint=0x55)
    http = probe.driver_required.http

    first = await http.upload_media(session.session_id, data, name="one.png")
    object_path = media_object_path(probe, session.session_id, first["media"]["id"])
    before = object_path.stat()

    second = await http.upload_media(session.session_id, data, name="two.png")

    assert second["media"]["id"] == first["media"]["id"], second
    assert second["media"]["name"] == "two.png", second
    after = object_path.stat()
    assert (after.st_ino, after.st_mtime_ns, after.st_size) == (
        before.st_ino,
        before.st_mtime_ns,
        before.st_size,
    ), "二次上传重写了对象文件（应幂等跳过）"
    assert object_names(probe, session.session_id) == [first["media"]["id"]]


# ── 场景 3：拒绝零副作用 ─────────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_rejected_uploads_leave_no_trace(probe: Probe) -> None:
    """非图 / 空 body / 头部截断 → 400，媒体目录不存在；随后合法上传照常。"""
    session = await probe.session(model=VISION_MODEL)
    http = probe.driver_required.http
    good = png(2, 2, tint=0x66)

    for bad, marker in (
        (b"", "empty request body"),
        (b"definitely not an image", "not a supported image"),
        (good[:20], "cannot read png image dimensions"),
    ):
        with pytest.raises(DriverHttpError) as failure:
            await http.upload_media(session.session_id, bad)
        call = failure.value.call
        assert call.status == 400, call.render()
        assert marker in str((call.response or {}).get("detail", "")), call.render()
        assert not media_dir(probe, session.session_id).exists(), (
            "400 路径在存储上留下了痕迹"
        )

    # 闸门没有把会话 / 存储弄坏：合法字节随后照常入库。
    response = await http.upload_media(session.session_id, good)
    assert object_names(probe, session.session_id) == [response["media"]["id"]]


@pytest.mark.probe_env(models=[VISION_SPEC], images=SMALL_LIMIT_IMAGES)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_oversize_upload_is_rejected_by_config_limit(probe: Probe) -> None:
    """超 ``images.max_bytes``（注入 150 B）→ 400；小图仍可上传、目录不增对象。"""
    session = await probe.session(model=VISION_MODEL)
    http = probe.driver_required.http
    small = png(2, 2, tint=0x77)
    big = png_padded(PADDED_PNG_BYTES, tint=0x88)
    assert len(small) <= SMALL_LIMIT_IMAGES["max_bytes"] < len(big)

    accepted = await http.upload_media(session.session_id, small)
    assert object_names(probe, session.session_id) == [accepted["media"]["id"]]

    with pytest.raises(DriverHttpError) as failure:
        await http.upload_media(session.session_id, big)
    call = failure.value.call
    assert call.status == 400, call.render()
    detail = str((call.response or {}).get("detail", ""))
    assert "image too large" in detail and "images.max_bytes" in detail, detail
    assert object_names(probe, session.session_id) == [accepted["media"]["id"]], (
        "超限上传在会话媒体目录里留下了对象"
    )


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_ghost_session_upload_is_404(probe: Probe) -> None:
    """会话不存在（含路径穿越式 id）→ 404，且不产生任何目录残留。"""
    http = probe.driver_required.http
    data = png(2, 2, tint=0x99)

    for ghost in ("ghost-session-upload", "../escape-upload"):
        # 会话存在性先于内容校验（与 send 同序）：body 非法也回 404，不泄漏"校验差一步"。
        for body in (data, b"not an image at all"):
            with pytest.raises(DriverHttpError) as failure:
                await http.upload_media(ghost, body)
            call = failure.value.call
            assert call.status == 404, call.render()
            assert (call.response or {}).get("detail") == "session not found", (
                call.render()
            )

    assert not probe.env.session_dir("ghost-session-upload").exists()
    assert not (probe.env.sessions_path.parent / "escape-upload").exists()


# ── 场景 4：端到端红线（上传 → 引用 → 模型请求体） ────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_uploaded_bytes_reach_the_model_request(probe: Probe) -> None:
    """上传 → 发消息带 ref → 请求体 image part 解码逐字节 == 上传字节；history 无 base64。"""
    probe.register(VISION_MODEL, Turn.of(text="I can see it"))
    session = await probe.session(model=VISION_MODEL)
    http = probe.driver_required.http
    data = png(3, 2, tint=0xAA)

    uploaded = await http.upload_media(session.session_id, data, name="pasted.png")
    ref = uploaded["media"]

    await session.send("look at this", media=[ref])
    result = await session.watch.expect(["turn_result", "error"], within=30)
    assert result.type == "turn_result", result.data
    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    # ① 请求体：user 消息 = [原文本 part, image part]；解码 == 上传字节。
    context = probe.context(VISION_MODEL, 0)
    users = user_messages(context.body)
    assert len(users) == 1, context.describe()
    parts = content_parts(users[0])
    assert [part["type"] for part in parts] == ["text", "image_url"], parts
    assert parts[0]["text"] == "look at this", parts
    assert image_bytes(parts[1]) == data

    # ② history：引用进链、绝不落 base64。
    view = probe.history(session)
    records = [m for m in view.messages() if m["role"] == "user"]
    assert len(records) == 1, view.describe()
    assert records[0]["media"] == [ref], records[0]
    assert_no_base64_in_history(session.session_id, probe, data)
    view.assert_chain_invariants()

    # ③ 存储：对象仍在会话目录（引用可兑现）。
    assert media_object_path(probe, session.session_id, ref["id"]).read_bytes() == data


# ── 场景 5：name 可选 ───────────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_upload_without_name_is_referencable(probe: Probe) -> None:
    """不带 name → 响应 name 为 null；对象照常可被引用（name 不是寻址键）。"""
    probe.register(VISION_MODEL, Turn.of(text="nameless but seen"))
    session = await probe.session(model=VISION_MODEL)
    http = probe.driver_required.http
    data = png(2, 2, tint=0xBB)

    uploaded = await http.upload_media(session.session_id, data)
    assert uploaded["media"]["name"] is None, uploaded
    assert uploaded["media"]["id"] == sha256(data), uploaded

    await session.send("no name here", media=[uploaded["media"]])
    result = await session.watch.expect(["turn_result", "error"], within=30)
    assert result.type == "turn_result", result.data
    assert result.data["subtype"] == "success", result.data

    context = probe.context(VISION_MODEL, 0)
    parts = content_parts(user_messages(context.body)[0])
    assert [part["type"] for part in parts] == ["text", "image_url"], parts
    assert image_bytes(parts[1]) == data

    wire = json.dumps(context.body, ensure_ascii=False)
    assert "image_url" in wire  # 图确实以 image part 发射（不是占位）


# ── 场景 6：别名拼写（大小写不敏感 FS）写进真名目录 ───────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_alias_spelling_lands_in_the_canonical_session_dir(probe: Probe) -> None:
    """别名拼写上传：对象必须落**磁盘真名**目录，不能另起第二个会话目录。

    大小写不敏感的 FS（macOS APFS / 开发机）上真别名成立：`_resolve_with_store`
    返回真名 → 写目标跟着真名（别名不分裂会话）。大小写敏感的 FS（CI 的 Linux）
    上别名**不是同一个会话**——按"不存在"处理（404）、同样不得留下第二份目录。
    两个分支各自断言（比 skip 强：敏感 FS 也能守住"不另起目录"）。
    """
    probe.register(VISION_MODEL, Turn.of(text="seeded"))
    session = await probe.session(model=VISION_MODEL)
    # 会话要真的"是会话"（`_resolve_with_store` 的 exists 判据 = metadata / history）：
    # 首轮对话落链——刚建的空会话连逐字拼写都不在磁盘事实里。
    await session.chat("seed the history")
    sid = session.session_id
    alias = sid.swapcase()
    assert alias != sid, sid  # 自动 id（YYYYMMDD-HHMMSS-hex）必有可翻的大小写
    data = png(2, 2, tint=0xDD)
    http = probe.driver_required.http

    if not fs_is_case_insensitive(probe.env.sessions_path):
        with pytest.raises(DriverHttpError) as failure:
            await http.upload_media(alias, data)
        assert failure.value.call.status == 404, failure.value.call.render()
        assert session_dirs(probe) == [sid], session_dirs(probe)
        return

    uploaded = await http.upload_media(alias, data)
    assert uploaded["media"]["id"] == sha256(data), uploaded
    # 目录清单里没有第二个变体目录；对象落在真名目录里。
    assert session_dirs(probe) == [sid], session_dirs(probe)
    assert (
        media_object_path(probe, sid, uploaded["media"]["id"]).read_bytes() == data
    ), "别名上传的对象没有落进磁盘真名目录"
