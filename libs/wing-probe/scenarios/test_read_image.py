"""read-image 链路 probe 场景（step 06 / 07 / 03-media-cap）。

覆盖 12 条：正向读图（默认 followup）/ inline 形态 / 能力门禁 / 换模型降级（红线）/
高水位量子驱逐（红线）/ 压缩剥离（红线）/ fork 后媒体可用 / 存量回放兼容 /
provider `image_max_bytes` 请求期降级 / `images.max_bytes` 读时拒绝（注入覆盖）/
字节缺失降级（UNAVAILABLE 占位）/ 默认上限读时拒绝（不注入覆盖）。

断言只锚定四个取证面：**事件时间线**（WS 事件 data）、**假 Provider 请求留档**
（`probe.context` / `probe.request`）、**history.jsonl**（独立解析）、**文件系统**
（workspace 文件 + `<sessions-root>/.media/<id[:2]>/<id>` 对象文件）。图片字节由
场景现场生成（stdlib zlib + struct 构造的合法 PNG），bytes / width / height / sha256
与被测文件同源，不使用大 base64 常量。

占位文案 / 引导文案是**协议常量**：这里按字面量断言（不 import wing）——它们是模型
可见输出的一部分，改动必须让本文件变红。
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import struct
import time
import zlib
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import pytest

from wing_probe import Probe, Session, ToolCall, Turn

# ── 模型名 / 配置（剧本按 model 名路由，场景之间零共享） ──────────────

VISION_MODEL = "probe/read-image-vision"
TEXT_MODEL = "probe/read-image-text"

VISION_SPEC: dict[str, Any] = {
    "name": VISION_MODEL,
    "display_name": "Read Image Vision",
    "description": "declares vision capability",
    "capabilities": {"vision": True},
}
TEXT_SPEC: dict[str, Any] = {
    "name": TEXT_MODEL,
    "display_name": "Text Only",
    "capabilities": {"vision": False},
}

# ── 协议常量（frozen strings；见 wing/media.py 与 wing/provider/media.py） ──
#
# 这里按**字面量**断言：AST 门禁（`wing_probe/guard.py`）禁止 probe 侧
# `import wing`，实现常量不可导入——本文件是外部实现，字面量即线上的事实。
# 每条与实现逐字节相同；任何漂移（含单字符）都会让本文件变红。

PLACEHOLDER_NO_VISION = "(image omitted: this model does not accept image input)"
PLACEHOLDER_BUDGET = (
    "(image omitted from this request: context image budget; "
    "re-read the file to attach it again)"
)
PLACEHOLDER_TOO_LARGE = "(image omitted: exceeds this provider's per-image size limit)"
PLACEHOLDER_UNAVAILABLE = (
    "(image unavailable: stored image bytes could not be read; "
    "re-read the file to attach it again)"
)
PLACEHOLDERS = frozenset(
    {
        PLACEHOLDER_NO_VISION,
        PLACEHOLDER_BUDGET,
        PLACEHOLDER_TOO_LARGE,
        PLACEHOLDER_UNAVAILABLE,
    }
)
FOLLOWUP_GUIDE = "Images read by the preceding tool results are attached below."

#: 逐出场景的环境旋钮（与 test_session_eviction.py 同款：TTL 1s / 扫描 0.5s）。
FAST_EVICTION: dict[str, Any] = {
    "eviction": {"idle_ttl_seconds": 1.0, "sweep_interval_seconds": 0.5}
}

#: 高水位量子驱逐场景的 images 配置（4 张图 → 计数超 1 → 按量子 2 批量丢最旧）。
EVICTION_IMAGES: dict[str, Any] = {"max_images": 3, "count_quantum": 2}

#: 单图字节上限场景的旋钮值：64×64 的 `png()` 输出 197 B —— 超过 cap 且两端都
#: < 1 KiB（`format_size` 走 "N bytes" 字面形态，断言不依赖 KB 舍入）。
IMAGE_BYTES_CAP = 100

#: 默认上限场景的文件尺寸：5 MiB —— 超过默认 `images.max_bytes`（4.5 MiB /
#: 4_718_592 B）且人类可读形态可区分（"5.0 MB exceeds the 4.5 MB per-image limit"）。
DEFAULT_CAP_PROBE_BYTES = 5 * 1024 * 1024

#: 状态翻转的轮询预算（TTL 1s + 扫描 0.5s，留足抖动余量）。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.2


# ── 图片与消息助手 ────────────────────────────────────────────────


def png(width: int = 2, height: int = 2, *, tint: int = 0xE0) -> bytes:
    """合法的最小 PNG（8-bit RGB、无 filter、真 IDAT/IEND 与 CRC）。

    `tint` 参与像素内容——同尺寸不同 tint 得到不同 sha256（场景需要多张互异图）。
    """

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
    """把图片字节写进 probe workspace，返回解析后的绝对路径（工具信封里就是这个）。"""
    path = probe.workspace / name
    path.write_bytes(data)
    return path


def png_padded(size: int, *, tint: int = 0xCC) -> bytes:
    """合法 PNG 尾部补零到恰 `size` 字节（IEND 之后的尾随字节不参与解析）。

    默认上限场景只探**尺寸门**（`os.stat` 先于读字节，内容不参与判定）——
    补零让文件保持"确实是图片"且尺寸精确可控（不必真造 5 MiB 像素数据）。
    """
    base = png(2, 2, tint=tint)
    assert size > len(base)
    return base + b"\x00" * (size - len(base))


def media_id(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def envelope_text(path: Path, data: bytes, width: int, height: int) -> str:
    """工具信封的期望文本（< 1 KiB → "N bytes" 形态；见 format_size）。"""
    ref_id = media_id(data)
    return (
        f"[image: {path} | png {width}x{height} | {len(data)} bytes | "
        f"id {ref_id[:8]} | mtime {int(path.stat().st_mtime)}]"
    )


def expected_ref(name: str, data: bytes, width: int, height: int) -> dict[str, Any]:
    """MediaRef.model_dump() 的期望形状（事件 tool_media / history 记录共用）。"""
    return {
        "id": media_id(data),
        "mime": "image/png",
        "bytes": len(data),
        "width": width,
        "height": height,
        "name": name,
    }


def content_parts(message: dict[str, Any]) -> list[dict[str, Any]]:
    """消息 content → part 列表（str 视为单个 text part；空串 → 空表）。"""
    content = message.get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}] if content else []
    parts = content if isinstance(content, list) else []
    return [part for part in parts if isinstance(part, dict)]


def image_parts(message: dict[str, Any]) -> list[dict[str, Any]]:
    return [part for part in content_parts(message) if part.get("type") == "image_url"]


def placeholder_texts(message: dict[str, Any]) -> list[str]:
    return [
        str(part["text"])
        for part in content_parts(message)
        if part.get("type") == "text" and part.get("text") in PLACEHOLDERS
    ]


def image_bytes(part: dict[str, Any]) -> bytes:
    """image_url data URL → 原始字节。"""
    url = str(part["image_url"]["url"])
    prefix, payload = url.split(",", 1)
    assert prefix.startswith("data:image/png;base64"), prefix
    return base64.b64decode(payload)


def messages_of(request_body: Mapping[str, Any]) -> list[dict[str, Any]]:
    messages = request_body["messages"]
    assert isinstance(messages, list), request_body
    return [message for message in messages if isinstance(message, dict)]


def media_signature(request_body: Mapping[str, Any]) -> list[tuple[str, ...]]:
    """整条请求的「媒体形态签名」：每条含图 / 占位的消息 → part 级签名。

    image part → 解码后 sha256[:8]；占位文本 → 文案原文。投影确定性的对账口径：
    同一媒体集合的两条请求应得到同一签名（与消息数量、有无新文本无关）。
    """
    signature: list[tuple[str, ...]] = []
    for message in messages_of(request_body):
        entries: list[str] = []
        for part in content_parts(message):
            if part.get("type") == "image_url":
                entries.append("image:" + media_id(image_bytes(part))[:8])
            elif part.get("type") == "text" and part.get("text") in PLACEHOLDERS:
                entries.append("placeholder:" + str(part["text"]))
        if entries:
            signature.append(tuple(entries))
    return signature


def bearer_messages(
    request_body: Mapping[str, Any],
) -> list[tuple[int, dict[str, Any]]]:
    """带图消息（消息下标 + 消息本体）。"""
    return [
        (index, message)
        for index, message in enumerate(messages_of(request_body))
        if image_parts(message)
    ]


def tool_messages(request_body: Mapping[str, Any]) -> list[dict[str, Any]]:
    return [
        message
        for message in messages_of(request_body)
        if message.get("role") == "tool"
    ]


def media_object_path(probe: Probe, ref_id: str) -> Path:
    """会话媒体池对象：`<sessions-root>/.media/<id[:2]>/<id>`。"""
    return probe.env.sessions_path / ".media" / ref_id[:2] / ref_id


async def _session_status(probe: Probe, session_id: str) -> str | None:
    """``/api/session/list`` 里该会话的运行时状态（None = 不在列表里）。"""
    payload = await probe.driver_required.http.request("GET", "/api/session/list")
    for entry in payload.get("sessions", []):
        if entry.get("id") == session_id:
            return entry.get("status")
    return None


async def _wait_status(probe: Probe, session_id: str, expected: str) -> str:
    """轮询到状态等于 ``expected``（超时即失败，报告带最后观察值）。"""
    deadline = time.monotonic() + POLL_DEADLINE
    observed: str | None = None
    while time.monotonic() < deadline:
        observed = await _session_status(probe, session_id)
        if observed == expected:
            return expected
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session {session_id} stayed in status {observed!r} for "
        f"{POLL_DEADLINE:.0f}s, expected {expected!r}"
    )


def assert_no_media_in_history(session: Session, probe: Probe, data: bytes) -> None:
    """history.jsonl 只存引用、绝不落 base64（逐字面对账）。"""
    raw = session.history.path.read_text(encoding="utf-8")
    assert base64.b64encode(data).decode() not in raw, "history 里落进了 base64 字节"
    assert "image_url" not in raw and "data:image" not in raw, raw[-400:]


# ── 场景 1：正向读图（vision 模型，默认 followup） ───────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_positive_read_default_followup(probe: Probe) -> None:
    """正向读图：事件 / 请求 / history / 存储四层取证（spec 场景 1）。"""
    data = png(3, 2, tint=0x11)
    path = write_image(probe, "pic.png", data)
    envelope = envelope_text(path, data, 3, 2)
    ref = expected_ref("pic.png", data, 3, 2)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "pic.png"})]),
        Turn.of(text="I can see it"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    result = await session.chat("look at pic.png")
    assert result.data["subtype"] == "success", result.data

    # ① 事件：tool_call_result 带 tool_media（逐字段 == 文件事实），信封 == 工具文本。
    call_events = session.watch.events(
        "tool_call_result", where={"tool_name": "ReadImage"}
    )
    assert len(call_events) == 1, [event.index for event in call_events]
    call_event = call_events[0]
    assert call_event.data["tool_success"] is True, call_event.data
    assert call_event.data["tool_result"] == envelope, call_event.data["tool_result"]
    assert call_event.data["tool_media"] == [ref], call_event.data["tool_media"]
    session.watch.assert_ordered(
        ["tool_call", "tool_call_result", "turn_result"], since=0
    )
    session.watch.assert_never("error")

    # ② 请求（默认 followup）：tool 消息只留文本；图片汇总为段后一条 user 消息，
    #    data URL 解码后 sha256 == 文件 sha256。
    context = probe.context(VISION_MODEL, 1)
    body = context.body
    tools = tool_messages(body)
    assert len(tools) == 1, context.describe()
    assert tools[0]["content"] == envelope, tools[0]  # 字符串形态、逐字节未改

    bearers = bearer_messages(body)
    assert len(bearers) == 1, context.describe()
    position, message = bearers[0]
    assert message["role"] == "user", message
    assert position > messages_of(body).index(tools[0]), context.describe()
    guide, *image_block = content_parts(message)
    # 引导文案是首块；provider 的显式缓存模式可能给末位文本块附加
    # `cache_control`（图片不落标记，回退到其前面的文本块），这里只锚定语义字段。
    assert guide["type"] == "text" and guide["text"] == FOLLOWUP_GUIDE, guide
    assert [image_bytes(part) for part in image_block] == [data], image_block

    # ③ history：tool 记录带 media 引用；文件里没有 base64 / data URL。
    history = probe.history(session)
    tool_records = [m for m in history.messages() if m["role"] == "tool"]
    assert len(tool_records) == 1, history.describe()
    assert tool_records[0]["media"] == [ref], tool_records[0]
    assert_no_media_in_history(session, probe, data)
    history.assert_chain_invariants()
    history.assert_no_transient_records()

    # ④ 存储：内容寻址对象落盘，sha256(对象内容) == id。
    object_path = media_object_path(probe, ref["id"])
    assert object_path.is_file(), sorted(
        item.relative_to(probe.env.sessions_path).as_posix()
        for item in probe.env.sessions_path.rglob("*")
    )
    assert media_id(object_path.read_bytes()) == ref["id"]


# ── 场景 2：inline 形态（provider_extra 透传旋钮） ────────────────────


@pytest.mark.probe_env(
    models=[VISION_SPEC], provider_extra={"image_delivery": "inline"}
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_inline_delivery_keeps_image_in_tool_message(probe: Probe) -> None:
    """inline：图片 part 留在 tool 消息的 content 数组内（紧跟文本 part）。"""
    data = png(2, 2, tint=0x22)
    path = write_image(probe, "inline.png", data)
    envelope = envelope_text(path, data, 2, 2)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "inline.png"})]),
        Turn.of(text="inline ok"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    context = probe.context(VISION_MODEL, 1)
    request_json = json.dumps(context.body, ensure_ascii=False)
    tools = tool_messages(context.body)
    assert len(tools) == 1, context.describe()
    parts = content_parts(tools[0])
    assert [part["type"] for part in parts] == ["text", "image_url"], parts
    assert parts[0]["text"] == envelope, parts[0]
    assert image_bytes(parts[1]) == data

    # inline 形态不产生段后 user 消息；引导文案不出现。
    bearers = bearer_messages(context.body)
    assert [position for position, _ in bearers] == [
        messages_of(context.body).index(tools[0])
    ], context.describe()
    assert FOLLOWUP_GUIDE not in request_json


# ── 场景 3：能力门禁（vision: false 的模型） ─────────────────────────


@pytest.mark.probe_env(models=[TEXT_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_vision_gate_refuses_without_side_effects(probe: Probe) -> None:
    """能力门禁：拒绝文案 + 请求无 image part + 媒体池零写入（spec 场景 3）。"""
    data = png(2, 2, tint=0x33)
    write_image(probe, "nope.png", data)

    probe.register(
        TEXT_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "nope.png"})]),
        Turn.of(text="cannot see it"),
    )
    session = await probe.session(model=TEXT_MODEL, tools=["ReadImage"])
    result = await session.chat("look at nope.png")
    assert result.data["subtype"] == "success", result.data

    call_events = session.watch.events(
        "tool_call_result", where={"tool_name": "ReadImage"}
    )
    assert len(call_events) == 1, [event.index for event in call_events]
    call_event = call_events[0]
    assert call_event.data["tool_success"] is False, call_event.data
    message = call_event.data["tool_result"]
    assert "does not declare vision capability" in message, message
    assert TEXT_MODEL in message, message
    assert "No file was read." in message, message
    assert call_event.data["tool_media"] == [], call_event.data

    # 请求里没有任何 image part（含后续轮次）。
    for entry in probe.requests_for(TEXT_MODEL):
        assert "image_url" not in json.dumps(entry.body), entry.describe()

    # 拒绝不产生副作用：媒体池对象目录整个不存在。
    assert not (probe.env.sessions_path / ".media").exists()


# ── 场景 4：换模型降级（红线） ─────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC, TEXT_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_model_switch_downgrades_to_placeholder(probe: Probe) -> None:
    """换模型降级：图片位变占位文本、无 image part、原信封 part 逐字节未改。"""
    data = png(4, 3, tint=0x44)
    path = write_image(probe, "switch.png", data)
    envelope = envelope_text(path, data, 4, 3)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "switch.png"})]),
        Turn.of(text="seen with vision"),
    )
    probe.register(TEXT_MODEL, Turn.of(text="text-only reply"))
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    # 切换前基线：图片作为 image part 发射。
    before = probe.context(VISION_MODEL, 1)
    before_tools = tool_messages(before.body)
    assert len(before_tools) == 1 and isinstance(before_tools[0]["content"], str)
    assert before_tools[0]["content"] == envelope
    assert len(bearer_messages(before.body)) == 1, before.describe()

    # 切到 text-only 模型（同一 provider 内的另一个模型名；该端点要求
    # model 与 provider 成对给出）。
    updated = await probe.driver_required.http.request(
        "POST",
        "/api/session/update",
        body={
            "session_id": session.session_id,
            "model": TEXT_MODEL,
            "provider": "probe",
        },
    )
    assert updated.get("ok") is True, updated

    result = await session.chat("what now")
    assert result.data["subtype"] == "success", result.data

    after = probe.context(TEXT_MODEL, 0)
    assert after.model == TEXT_MODEL, after.describe()
    assert not bearer_messages(after.body), after.describe()
    after_tools = tool_messages(after.body)
    assert len(after_tools) == 1, after.describe()
    parts = content_parts(after_tools[0])
    assert [part["type"] for part in parts] == ["text", "text"], parts
    # 红线：原文本 part 与上一轮请求里的字符串逐字节相同，只追加占位。
    assert parts[0]["text"] == envelope == before_tools[0]["content"], parts[0]
    assert parts[1]["text"] == PLACEHOLDER_NO_VISION, parts


# ── 场景 5：高水位量子驱逐（红线） ─────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC], images=EVICTION_IMAGES)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_high_water_quantum_eviction(probe: Probe) -> None:
    """高水位量子驱逐：连读 4 张（max_images=3 / count_quantum=2）→ 仅最新两张为图。

    数量账：excess = 4 - 3 = 1 → ceil(1/2)*2 = 2 条被丢（最旧优先），恰余 2 张。
    确定性：同一媒体集合下连续两次请求的媒体形态签名全等。
    """
    names = ["h1.png", "h2.png", "h3.png", "h4.png"]
    images = [(name, png(2, 2, tint=0x50 + index)) for index, name in enumerate(names)]
    envelopes = [
        envelope_text(write_image(probe, name, data), data, 2, 2)
        for name, data in images
    ]
    refs = [media_id(data) for _, data in images]

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": name}) for name in names]),
        Turn.of(text="four images"),
        Turn.of(text="again"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("read four")

    context = probe.context(VISION_MODEL, 1)
    tools = tool_messages(context.body)
    assert len(tools) == 4, context.describe()
    # tool 消息顺序 == 模型 tool_calls 顺序（信封文本逐条对上）。
    assert [content_parts(message)[0]["text"] for message in tools] == envelopes

    bearers = bearer_messages(context.body)
    assert len(bearers) == 1, context.describe()
    kept = [media_id(image_bytes(part)) for part in image_parts(bearers[0][1])]
    assert kept == refs[2:], (kept, refs)  # 最新两张保留

    for position, message in enumerate(tools):
        parts = content_parts(message)
        assert image_parts(message) == [], message  # followup：tool 消息不带图
        if position < 2:
            # 被驱逐的两条：原信封 part 不动，其后追加固定占位文本。
            assert [part["type"] for part in parts] == ["text", "text"], parts
            assert parts[0]["text"] == envelopes[position], parts[0]
            assert parts[1]["text"] == PLACEHOLDER_BUDGET, parts
        else:
            assert [part["type"] for part in parts] == ["text"], parts

    # 确定性：再发一轮，媒体形态签名与上一轮请求全等（无持久状态、纯函数）。
    result = await session.chat("again")
    assert result.data["subtype"] == "success", result.data
    second = probe.context(VISION_MODEL, 2)
    signature = media_signature(context.body)
    flat = [entry for entries in signature for entry in entries]
    assert sum(entry.startswith("image:") for entry in flat) == 2, flat
    assert sum(entry.startswith("placeholder:") for entry in flat) == 2, flat
    assert media_signature(second.body) == signature, (
        second.describe(),
        context.describe(),
    )


# ── 场景 6：压缩剥离（红线） ───────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_compact_request_strips_media(probe: Probe) -> None:
    """压缩剥离：compact 请求体无任何 image part / 占位；压缩后仍可续跑。"""
    data = png(3, 3, tint=0x66)
    path = write_image(probe, "compact.png", data)
    envelope = envelope_text(path, data, 3, 3)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "compact.png"})]),
        Turn.of(text="seen"),
        Turn.of(text="<summary>Task: keep talking.</summary>"),
        Turn.of(text="post compact"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    # 基线：压缩前，媒体确实在线上（否则"剥离"断言没有意义）。
    assert "image_url" in json.dumps(probe.request(VISION_MODEL, 1).body)

    await session.compact()
    await session.watch.expect("compact_done", within=15)
    assert probe.requests.count(VISION_MODEL) == 3, probe.requests.summary()

    # 压缩请求（非流式）：纯文本序列化——无 image part，也无任何占位文案。
    compact_request = probe.request(VISION_MODEL, 2)
    assert compact_request.stream is False, compact_request.describe()
    compact_json = json.dumps(compact_request.body, ensure_ascii=False)
    assert "image_url" not in compact_json, compact_json[-400:]
    for placeholder in PLACEHOLDERS:
        assert placeholder not in compact_json, placeholder
    compact_tools = tool_messages(compact_request.body)
    assert compact_tools and all(
        isinstance(message["content"], str) for message in compact_tools
    ), compact_tools
    assert compact_tools[0]["content"] == envelope, compact_tools[0]

    # 压缩后仍可续跑；压缩把媒体引用随旧链移出活跃链 → 之后再无 image part。
    result = await session.chat("continue")
    assert result.data["subtype"] == "success", result.data
    after = probe.context(VISION_MODEL, 3)
    assert not bearer_messages(after.body), after.describe()
    assert not any(placeholder_texts(m) for m in messages_of(after.body))


# ── 场景 7：fork 后媒体可用 ────────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_fork_keeps_media_usable(probe: Probe) -> None:
    """fork 后媒体可用：子会话请求里图片仍是 image part（引用池共享，无拷贝）。"""
    data = png(2, 3, tint=0x77)
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

    source = probe.history(session)
    target = source.messages()[-1]  # 末条 assistant：子链恰好保留到 tool 结果
    child = await session.fork(target["uuid"])
    child_view = probe.history(child)
    assert child_view.messages()[-1]["role"] == "tool", child_view.describe()
    assert child_view.messages()[-1]["media"][0]["id"] == ref_id, child_view.describe()

    result = await child.chat("continue")
    assert result.data["subtype"] == "success", result.data

    assert probe.requests.count(VISION_MODEL) == 3, probe.requests.summary()
    context = probe.context(VISION_MODEL, 2)  # 前两次属于源会话
    bearers = bearer_messages(context.body)
    assert len(bearers) == 1, context.describe()
    assert [media_id(image_bytes(part)) for part in image_parts(bearers[0][1])] == [
        ref_id
    ], context.describe()
    assert not any(placeholder_texts(m) for m in messages_of(context.body))

    # 存储：对象仍在那一个内容寻址路径上（fork 只共享引用，绝不拷贝字节）。
    media_dir = probe.env.sessions_path / ".media"
    objects = sorted(
        item.relative_to(media_dir).as_posix()
        for item in media_dir.rglob("*")
        if item.is_file()
    )
    assert objects == [f"{ref_id[:2]}/{ref_id}"], objects


# ── 场景 8：存量回放兼容（history.jsonl 无 media 键） ────────────────


@pytest.mark.probe_env(models=[VISION_SPEC], sessions=FAST_EVICTION)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_history_without_media_resumes(probe: Probe) -> None:
    """存量回放兼容：旧记录（无 media 键）逐出后 resume + 续跑正常。"""
    data = png(2, 2, tint=0x88)
    write_image(probe, "old.png", data)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "old.png"})]),
        Turn.of(text="seen"),
        Turn.of(text="legacy reply"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    before = probe.history(session)
    assert [message["role"] for message in before.messages()] == [
        "user",
        "assistant",
        "tool",
        "assistant",
    ], before.describe()
    assert before.messages()[2]["media"], before.describe()

    # 逐出：退订 → 状态转 inactive（磁盘成为唯一事实来源）。
    driver = probe.driver_required
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    await _wait_status(probe, session.session_id, "inactive")

    # 模拟引入 media 之前的旧记录：裁掉 media 键，其余行逐字节保留。
    history_path = session.session_dir / "history.jsonl"
    stripped = 0
    rewritten: list[str] = []
    for line in history_path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        if "media" in record:
            del record["media"]
            stripped += 1
            rewritten.append(json.dumps(record, ensure_ascii=False))
        else:
            rewritten.append(line)
    assert stripped == 1, stripped
    history_path.write_text(
        "".join(f"{line}\n" for line in rewritten), encoding="utf-8"
    )

    # resume（重新水合）+ 显式重新订阅（resume 端点不代订阅；驱动句柄已在驱动
    # 注册表中，attach 复用句柄不会重复订阅——这是既有场景的成熟写法）。
    resumed = await probe.resume(session.session_id)
    await driver.http.subscribe(session.session_id, driver.client_id)
    view = probe.history(resumed)
    assert [message["role"] for message in view.messages()] == [
        "user",
        "assistant",
        "tool",
        "assistant",
    ], view.describe()
    assert all("media" not in message for message in view.messages()), view.describe()

    result = await resumed.chat("continue")
    assert result.data["subtype"] == "success", result.data

    context = probe.last_request(VISION_MODEL).context()
    assert not bearer_messages(context.body), context.describe()
    assert not any(placeholder_texts(m) for m in messages_of(context.body))
    view.assert_chain_invariants()
    view.assert_tool_pairing()


# ── 场景 9：provider image_max_bytes（请求期兜底降级） ──────────────


@pytest.mark.probe_env(
    models=[VISION_SPEC], provider_extra={"image_max_bytes": IMAGE_BYTES_CAP}
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_provider_image_max_bytes_degrades_too_large(probe: Probe) -> None:
    """provider `image_max_bytes`：工具照常入库，请求期该位降级为 too_large 占位。

    与 `images.max_bytes`（读时拒绝）互补：这条链路只在请求期生效——工具仍是
    `tool_success=true` 且带引用、字节照常落盘，序列化层用占位文本替换图片
    （原信封 part 一个字节都不动）。
    """
    data = png(64, 64, tint=0x99)
    assert len(data) > IMAGE_BYTES_CAP, len(data)  # 前提：图片确实超过 cap
    path = write_image(probe, "cap.png", data)
    envelope = envelope_text(path, data, 64, 64)
    ref = expected_ref("cap.png", data, 64, 64)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "cap.png"})]),
        Turn.of(text="noted"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    result = await session.chat("look")
    assert result.data["subtype"] == "success", result.data

    # ① 工具侧不受影响：成功事件、引用照带、字节照常入内容寻址池。
    call_events = session.watch.events(
        "tool_call_result", where={"tool_name": "ReadImage"}
    )
    assert len(call_events) == 1, [event.index for event in call_events]
    call_event = call_events[0]
    assert call_event.data["tool_success"] is True, call_event.data
    assert call_event.data["tool_result"] == envelope, call_event.data["tool_result"]
    assert call_event.data["tool_media"] == [ref], call_event.data["tool_media"]
    object_path = media_object_path(probe, ref["id"])
    assert object_path.is_file(), sorted(
        item.relative_to(probe.env.sessions_path).as_posix()
        for item in probe.env.sessions_path.rglob("*")
    )
    assert media_id(object_path.read_bytes()) == ref["id"]

    # ② 请求期降级：原信封 part 不动，其后追加 too_large 占位；整条请求无 image part。
    context = probe.context(VISION_MODEL, 1)
    tools = tool_messages(context.body)
    assert len(tools) == 1, context.describe()
    parts = content_parts(tools[0])
    assert [part["type"] for part in parts] == ["text", "text"], parts
    assert parts[0]["text"] == envelope, parts[0]
    assert parts[1]["text"] == PLACEHOLDER_TOO_LARGE, parts
    assert not bearer_messages(context.body), context.describe()


# ── 场景 10：images.max_bytes（读时拒绝 + 零写入） ─────────────────


@pytest.mark.probe_env(models=[VISION_SPEC], images={"max_bytes": IMAGE_BYTES_CAP})
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_images_max_bytes_refuses_read_without_writing(probe: Probe) -> None:
    """`images.max_bytes`：超限图片在工具层被拒（文案含大小与上限）；媒体池零写入。"""
    data = png(64, 64, tint=0xAA)
    assert len(data) > IMAGE_BYTES_CAP, len(data)
    write_image(probe, "too-big.png", data)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "too-big.png"})]),
        Turn.of(text="understood"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    result = await session.chat("look")
    assert result.data["subtype"] == "success", result.data

    call_events = session.watch.events(
        "tool_call_result", where={"tool_name": "ReadImage"}
    )
    assert len(call_events) == 1, [event.index for event in call_events]
    call_event = call_events[0]
    assert call_event.data["tool_success"] is False, call_event.data
    message = call_event.data["tool_result"]
    # 文案把"实际大小 / 上限 / 配置键"都点名（两端 < 1 KiB → "N bytes" 形态）。
    assert (
        f"{len(data)} bytes exceeds the {IMAGE_BYTES_CAP} bytes per-image limit"
        in message
    ), message
    assert "images.max_bytes" in message, message
    assert call_event.data["tool_media"] == [], call_event.data

    # 拒绝发生在入库之前：媒体池目录整个不存在（零写入）。
    assert not (probe.env.sessions_path / ".media").exists()

    # 请求里也没有 image part / 占位（工具没产出引用，请求链上无任何媒体）。
    for entry in probe.requests_for(VISION_MODEL):
        body = json.dumps(entry.body)
        assert "image_url" not in body, entry.describe()
        for placeholder in PLACEHOLDERS:
            assert placeholder not in body, (entry.describe(), placeholder)


# ── 场景 11：字节缺失降级（UNAVAILABLE 占位） ─────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_missing_media_object_degrades_to_unavailable(probe: Probe) -> None:
    """存储故障：删除 `.media` 对象后，下一轮该位为 UNAVAILABLE 占位且请求照常完成。

    路径：读图入库（基线请求确认图片确实在线上）→ `unlink()` 内容寻址对象 →
    再发一轮 → 序列化层读到缺失 → WARN + 占位（不抛、不打断请求）。
    """
    data = png(2, 4, tint=0xBB)
    path = write_image(probe, "gone.png", data)
    envelope = envelope_text(path, data, 2, 4)
    ref_id = media_id(data)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "gone.png"})]),
        Turn.of(text="seen"),
        Turn.of(text="still fine"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.chat("look")

    # 基线：图片以 image part 发射（否则"降级"断言没有意义）。
    before = probe.context(VISION_MODEL, 1)
    assert len(bearer_messages(before.body)) == 1, before.describe()

    # 存储故障：对象文件被删除（工具早已把它写进内容寻址池）。
    object_path = media_object_path(probe, ref_id)
    assert object_path.is_file(), object_path
    object_path.unlink()

    # 再发一轮：该位降级为 UNAVAILABLE 占位，原信封文本不动，请求照常完成。
    result = await session.chat("again")
    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    context = probe.context(VISION_MODEL, 2)
    assert not bearer_messages(context.body), context.describe()
    tools = tool_messages(context.body)
    assert len(tools) == 1, context.describe()
    parts = content_parts(tools[0])
    assert [part["type"] for part in parts] == ["text", "text"], parts
    assert parts[0]["text"] == envelope, parts[0]
    assert parts[1]["text"] == PLACEHOLDER_UNAVAILABLE, parts


# ── 场景 12：默认上限（不注入 images 覆盖） ─────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_default_max_bytes_refuses_read_without_writing(probe: Probe) -> None:
    """**代码默认值** `images.max_bytes`（4.5 MiB）：超限读时拒绝、媒体池零写入。

    与场景 10 互补：那条注入 `images={"max_bytes": 100}` 探"配置生效"；这条
    不写 `images:` 段（`probe_env` 缺省即不注入）、探"缺省配置生效"——把默认值
    放宽回 8 MiB 会让 5 MiB 的文件被正常读入并入库，本场景立刻变红。
    """
    data = png_padded(DEFAULT_CAP_PROBE_BYTES)
    assert len(data) == DEFAULT_CAP_PROBE_BYTES
    write_image(probe, "over-default.png", data)

    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "over-default.png"})]),
        Turn.of(text="understood"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    await session.send("look")

    # ① 事件：读时拒绝（tool_success=false、无引用），文案点名实际大小 / 默认上限 /
    #    配置键——两个大小字符串必须可区分（5.0 MB vs 4.5 MB）。先于轮次结束断言：
    #    「默认值被放宽」这类变异会以"工具成功"的形态在这里直接变红，而不是绕到
    #    请求体超限的次生错误上。
    call_event = await session.watch.expect(
        "tool_call_result", where={"tool_name": "ReadImage"}, within=30
    )
    assert call_event.data["tool_success"] is False, call_event.data
    message = call_event.data["tool_result"]
    assert "5.0 MB exceeds the 4.5 MB per-image limit" in message, message
    assert "images.max_bytes" in message, message
    assert call_event.data["tool_media"] == [], call_event.data

    # ② 文件系统：拒绝发生在入库之前——媒体池目录整个不存在（零写入）。
    assert not (probe.env.sessions_path / ".media").exists()

    # 工具失败不打断轮次（正常收尾）。
    result = await session.watch.expect("turn_result", within=30)
    assert result.data["subtype"] == "success", result.data

    # ③ 请求留档：工具没产出引用 → 请求链上无 image part / 无占位文案。
    for entry in probe.requests_for(VISION_MODEL):
        body = json.dumps(entry.body)
        assert "image_url" not in body, entry.describe()
        for placeholder in PLACEHOLDERS:
            assert placeholder not in body, (entry.describe(), placeholder)
