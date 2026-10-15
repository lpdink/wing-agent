"""媒体存储布局 probe 场景（01 storage_layout）。

被守的语义（proposal D1：媒体对象是**会话私有**资产，session 自包含）：

- **新写入**落 `<sessions-root>/<sid>/media/<sha256>`（老共享池 `.media/` 不再是
  写入目标）；
- **跨机器拷贝**：整个会话目录拷进一个**全新 WING_HOME / sessions-root**（新网关
  进程、新假 Provider）→ resume → 请求体里的 image part 解码后**逐字节 == 原文件**
  ——对象只能来自会话目录（新环境里没有 `.media` 池）；
- **老池兼容 + 物化**：老布局对象（`<root>/.media/<id[:2]>/<id>`）+ 引用它的历史
  → 加载会话即把对象**物化**进会话目录（发消息之前就可断言）→ 请求照常带图；
  老对象**不删**；
- **fork 自包含**：fork 把被引用对象复制进子会话目录；删掉源会话的副本后，子会话
  请求仍成功（子目录独立可用）。

断言只锚定三个取证面：**假 Provider 请求留档**（请求体 image part 的字节）、
**文件系统**（对象落点 / 老池是否参与）、**history.jsonl**（引用 id）。图片字节由
场景现场生成（stdlib zlib + struct 构造的合法 PNG），独立成文件、不 import 其它
场景模块（probe 场景之间零共享）。
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import shutil
import struct
import time
import zlib
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import pytest

from wing_probe import Probe, ToolCall, Turn

# ── 模型 / 环境旋钮 ───────────────────────────────────────────────

VISION_MODEL = "probe/media-layout-vision"

VISION_SPEC: dict[str, Any] = {
    "name": VISION_MODEL,
    "display_name": "Media Layout Vision",
    "description": "declares vision capability",
    "capabilities": {"vision": True},
}

#: 逐出场景的环境旋钮（与 test_read_image.py 同款：TTL 1s / 扫描 0.5s）。
FAST_EVICTION: dict[str, Any] = {
    "eviction": {"idle_ttl_seconds": 1.0, "sweep_interval_seconds": 0.5}
}

POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.2


# ── 图片与请求体助手（本文件自足） ────────────────────────────────


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


def write_image(probe: Probe, name: str, data: bytes) -> Path:
    """把图片字节写进 probe workspace（ReadImage 的相对路径基准）。"""
    path = probe.workspace / name
    path.write_bytes(data)
    return path


def media_id(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def media_object_path(probe: Probe, session_id: str, ref_id: str) -> Path:
    """会话私有媒体对象：`<sessions-root>/<sid>/media/<id>`。"""
    return probe.env.session_dir(session_id) / "media" / ref_id


def content_parts(message: dict[str, Any]) -> list[dict[str, Any]]:
    content = message.get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}] if content else []
    parts = content if isinstance(content, list) else []
    return [part for part in parts if isinstance(part, dict)]


def image_parts(message: dict[str, Any]) -> list[dict[str, Any]]:
    return [part for part in content_parts(message) if part.get("type") == "image_url"]


def image_bytes(part: dict[str, Any]) -> bytes:
    """image_url data URL → 原始字节。"""
    url = str(part["image_url"]["url"])
    prefix, payload = url.split(",", 1)
    assert prefix.startswith("data:image/png;base64"), prefix
    return base64.b64decode(payload)


def bearer_messages(body: Mapping[str, Any]) -> list[dict[str, Any]]:
    """带图消息（请求体里 image part 所在的消息）。"""
    messages = body["messages"]
    assert isinstance(messages, list), body
    return [
        message
        for message in messages
        if isinstance(message, dict) and image_parts(message)
    ]


def bearer_image_bytes(body: Mapping[str, Any]) -> list[bytes]:
    """整条请求里全部 image part 的原始字节（出现序）。"""
    result: list[bytes] = []
    for message in bearer_messages(body):
        result.extend(image_bytes(part) for part in image_parts(message))
    return result


async def _wait_status(probe: Probe, session_id: str, expected: str) -> str:
    """轮询到 `/api/session/list` 里该会话达到 `expected` 状态。"""
    deadline = time.monotonic() + POLL_DEADLINE
    observed: str | None = None
    while time.monotonic() < deadline:
        payload = await probe.driver_required.http.request("GET", "/api/session/list")
        observed = next(
            (
                entry.get("status")
                for entry in payload.get("sessions", [])
                if entry.get("id") == session_id
            ),
            None,
        )
        if observed == expected:
            return expected
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session {session_id} stayed in status {observed!r} for "
        f"{POLL_DEADLINE:.0f}s, expected {expected!r}"
    )


# ── 场景 A：跨机器拷贝（整个会话目录进全新 WING_HOME） ─────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_session_dir_copies_to_fresh_home_with_images(
    probe: Probe, tmp_path: Path
) -> None:
    """会话目录 = 可搬运单元：拷进全新 WING_HOME → resume → 图片逐字节可用。

    "另一台机器"用第二个 ProbeEnv 模拟（全新临时 WING_HOME + 全新 sessions-root +
    新网关进程 + 新假 Provider）：新环境里**没有** `.media` 共享池，对象只能来自被
    拷贝的会话目录——这正是"共享池未拷贝导致图片丢失"的反面。
    """
    data = png(3, 2, tint=0x11)
    write_image(probe, "pic.png", data)
    ref_id = media_id(data)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "pic.png"})]),
        Turn.of(text="seen"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    # 前提：对象在会话目录内，老共享池整个不存在。
    source_object = media_object_path(probe, session.session_id, ref_id)
    assert source_object.read_bytes() == data, source_object
    assert not (probe.env.sessions_path / ".media").exists()

    other = await Probe.start(tmp_path / "machine-b", models=[VISION_SPEC])
    try:
        other.register(VISION_MODEL, Turn.of(text="from the copy"))
        shutil.copytree(
            probe.env.session_dir(session.session_id),
            other.env.sessions_path / session.session_id,
        )
        # 新环境里没有任何共享池：图片只能来自被拷贝的会话目录。
        assert not (other.env.sessions_path / ".media").exists()
        resumed = await other.resume(session.session_id)

        result = await resumed.chat("continue")
        assert result.data["subtype"] == "success", result.data

        context = other.context(VISION_MODEL, 0)
        assert bearer_image_bytes(context.body) == [data], context.describe()
    finally:
        await other.stop()


# ── 场景 B：老池兼容 + 物化 ───────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC], sessions=FAST_EVICTION)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_pool_object_is_materialized_on_load(probe: Probe) -> None:
    """老会话（对象只在 `<root>/.media/`）：加载即物化进会话目录，请求照常带图。

    步骤：读图入库（新布局）→ 逐出内存态 → 把对象**搬到**老布局（模拟迁移前落盘
    的老会话）→ resume → 发消息之前就能断言对象已回到 `<sid>/media/`（加载期物化）
    → 请求体 image part 逐字节 == 原文件；老对象保留（永不删除）。
    """
    data = png(2, 2, tint=0x22)
    write_image(probe, "old.png", data)
    ref_id = media_id(data)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "old.png"})]),
        Turn.of(text="seen"),
        Turn.of(text="after migration"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    session_object = media_object_path(probe, session.session_id, ref_id)
    assert session_object.read_bytes() == data, session_object

    # 逐出：退订 → 状态转 inactive（磁盘成为唯一事实来源）。
    driver = probe.driver_required
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    await _wait_status(probe, session.session_id, "inactive")

    # 模拟迁移前的老会话：对象搬到老共享池，会话目录里没有。
    legacy = probe.env.sessions_path / ".media" / ref_id[:2] / ref_id
    legacy.parent.mkdir(parents=True, exist_ok=True)
    shutil.move(str(session_object), str(legacy))
    assert session_object.exists() is False
    assert legacy.read_bytes() == data

    # 加载（resume）→ 物化：**发消息之前**对象已在会话目录里。
    resumed = await probe.resume(session.session_id)
    assert session_object.read_bytes() == data, sorted(
        item.relative_to(probe.env.session_dir(session.session_id)).as_posix()
        for item in probe.env.session_dir(session.session_id).rglob("*")
    )
    assert legacy.read_bytes() == data, "物化是复制：老对象不删"

    # 请求照常带图，字节逐字节 == 原文件（resume 端点不代订阅；句柄已在驱动
    # 注册表中，重新订阅才收得到本轮事件）。
    await driver.http.subscribe(session.session_id, driver.client_id)
    result = await resumed.chat("continue")
    assert result.data["subtype"] == "success", result.data
    context = probe.last_request(VISION_MODEL).context()
    assert bearer_image_bytes(context.body) == [data], context.describe()


# ── 场景 C：fork 自包含 ───────────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_fork_copies_media_into_child(probe: Probe) -> None:
    """fork 复制媒体进子会话目录：删掉源副本后子会话请求仍成功（自包含）。"""
    data = png(4, 4, tint=0x33)
    write_image(probe, "fork.png", data)
    ref_id = media_id(data)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "fork.png"})]),
        Turn.of(text="seen"),
        Turn.of(text="child reply"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    source_object = media_object_path(probe, session.session_id, ref_id)
    assert source_object.read_bytes() == data, source_object

    source_messages = probe.history(session).messages()
    target = source_messages[-1]  # 末条 assistant：子链保留到 tool 结果
    child = await session.fork(target["uuid"])
    child_object = media_object_path(probe, child.session_id, ref_id)
    assert child_object.read_bytes() == data, child_object
    # 独立副本（不是共享同一 inode）：子目录拷走即可用。
    assert child_object.stat().st_ino != source_object.stat().st_ino

    # 破坏源会话的副本（老共享池也不存在）→ 子会话请求仍成功。
    source_object.unlink()
    assert not (probe.env.sessions_path / ".media").exists()

    result = await child.chat("continue")
    assert result.data["subtype"] == "success", result.data
    context = probe.last_request(VISION_MODEL).context()
    assert bearer_image_bytes(context.body) == [data], context.describe()
