"""用户贴图（user 消息携带媒体引用）probe 场景（02 message_chain）。

被守的语义（服务端侧）：

- **端到端**：预置媒体对象 → 发一条携带 refs 的 user 消息 → 请求体里 user 消息的
  content 数组 = [原文本 part, image part]，data URL 解码**逐字节 == 对象字节**；
  history 只落引用、绝无 base64；
- **纯图消息合法**：content 为空、media 非空的消息照常进上下文（请求体只有一个
  image part、没有空 text part），turn 正常收口，`user_message_accepted` 照发
  （否则前端 pending 卡到 turn end）；
- **多消息合并**：同一轮 drain 出的多条带图消息合并成**一条** user 消息，media
  顺序 = 消息序 × 消息内序；
- **steer 阻断（D7）**：工具执行期间到达的 `[纯文本A, 带图B, 纯文本C]` → 只有 A 被
  steer 注入（note 进工具结果），B/C 连同其后的纯文本一起留在队列，下一轮完整、
  保序进入；
- **no_vision 附路径（D6）**：text 模型收到贴图 → 无 image part，占位文本携带对象
  保存路径（`<sessions-root>/<sid>/media/<id>`）；对象仍在（永不删除）；
- **memory 后端**：`media_path` 恒 None → 降级为"仍在会话中"文案（不报路径）；
- **budget 驱逐（D5）**：user 图与 tool 图混合超字节预算 → 按最旧驱逐，user 位用
  user 变体占位（"仍在会话中"），原文本 part 逐字节不动；
- **幽灵引用拒绝**：不存在的 ref 被拒（HTTP 400 / WS error 帧），不进历史、没有
  送达 ack；id 格式非法 / mime 不在支持列表同样 400；WS 错误帧携带客户端
  `request_id` 与 `session_id`（前端据此把拒绝关联回刚发出的那条消息）。

断言只锚定四个取证面：**假 Provider 请求留档**（请求体 part 与字节）、**事件时间线**
（`user_message_accepted` 的 `origin_request_id` / 占位事件 / 带帧层关联字段的 `error` 帧）、
**history.jsonl**（引用与无 base64）、**文件系统**（对象落点与存续）。图片字节由场景
现场生成（stdlib zlib + struct 构造的合法 PNG），独立成文件、不 import 其它场景模块。

占位文案是**协议常量**：按字面量断言（probe 侧禁止 import wing）——它们改了就变红。
"""

from __future__ import annotations

import base64
import hashlib
import json
import struct
import zlib
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import pytest

from wing_probe import DriverHttpError, Probe, Session, ToolCall, Turn

# ── 模型 / 环境旋钮（场景之间零共享） ────────────────────────────

VISION_MODEL = "probe/user-media-vision"
TEXT_MODEL = "probe/user-media-text"

VISION_SPEC: dict[str, Any] = {
    "name": VISION_MODEL,
    "display_name": "User Media Vision",
    "description": "declares vision capability",
    "capabilities": {"vision": True},
}
TEXT_SPEC: dict[str, Any] = {
    "name": TEXT_MODEL,
    "display_name": "User Media Text",
    "capabilities": {"vision": False},
}

# ── 冻结文案（本文件按字面量断言；与 wing/media/policy.py 逐字节相同） ──

NO_VISION_USER_PLACEHOLDER = (
    "(image omitted: this model does not accept image input; "
    "the image is saved at {path})"
)
NO_VISION_USER_PLACEHOLDER_NO_PATH = (
    "(image omitted: this model does not accept image input; "
    "the image is kept in the session)"
)
BUDGET_USER_PLACEHOLDER = (
    "(image omitted from this request: context image budget; "
    "the image is still in the session)"
)
BUDGET_TOOL_PLACEHOLDER = (
    "(image omitted from this request: context image budget; "
    "re-read the file to attach it again)"
)
NO_VISION_TOOL_PLACEHOLDER = "(image omitted: this model does not accept image input)"
IMAGE_ONLY_TITLE = "[image message]"
"""会话标题回退（wing/store/base.py 的 `IMAGE_ONLY_TITLE`）：链上只有纯图 user
消息时的会话名——没有它，这类会话会因"无名"在 /api/session/list 里被隐藏。"""

#: 字节预算驱逐场景的 images 配置：两张 96 B 图（encoded 各 128 B）合计超 200
#: → 目标释放 64 B（量化 64）→ 恰丢最旧一张。
BUDGET_IMAGES: dict[str, Any] = {
    "max_images": 0,
    "count_quantum": 2,
    "request_budget_bytes": 200,
    "evict_quantum_bytes": 64,
}

#: 96 字节整的合法 PNG（工具读图需要可解析头部；尾部补零不参与解析）。
PADDED_PNG_BYTES = 96


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


def write_image(probe: Probe, name: str, data: bytes) -> Path:
    """把图片字节写进 probe workspace（ReadImage 的相对路径基准）。"""
    path = probe.workspace / name
    path.write_bytes(data)
    return path


def media_id(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def media_ref(
    name: str, data: bytes, width: int = 2, height: int = 2
) -> dict[str, Any]:
    """``MediaRef`` 形状的上行引用（与 scene 现场生成的对象事实同源）。"""
    return {
        "id": media_id(data),
        "mime": "image/png",
        "bytes": len(data),
        "width": width,
        "height": height,
        "name": name,
    }


def seed_media(
    probe: Probe, session_id: str, data: bytes, *, name: str
) -> dict[str, Any]:
    """把媒体对象预置进会话媒体目录（02 尚无上传端点；布局是 01 的冻结契约）。"""
    ref = media_ref(name, data)
    target = media_object_path(probe, session_id, ref["id"])
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(data)
    return ref


def media_object_path(probe: Probe, session_id: str, ref_id: str) -> Path:
    """会话私有媒体对象：`<sessions-root>/<sid>/media/<id>`。"""
    return probe.env.session_dir(session_id) / "media" / ref_id


def content_parts(message: Mapping[str, Any]) -> list[dict[str, Any]]:
    """消息 content → part 列表（str 视为单个 text part；空串 → 空表）。"""
    content = message.get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}] if content else []
    parts = content if isinstance(content, list) else []
    return [part for part in parts if isinstance(part, dict)]


def image_parts(message: Mapping[str, Any]) -> list[dict[str, Any]]:
    return [part for part in content_parts(message) if part.get("type") == "image_url"]


def image_bytes(part: Mapping[str, Any]) -> bytes:
    """image_url data URL → 原始字节。"""
    url = str(part["image_url"]["url"])
    prefix, payload = url.split(",", 1)
    assert prefix.startswith("data:image/png;base64"), prefix
    return base64.b64decode(payload)


def messages_of(request_body: Mapping[str, Any]) -> list[dict[str, Any]]:
    messages = request_body["messages"]
    assert isinstance(messages, list), request_body
    return [message for message in messages if isinstance(message, dict)]


def user_messages(request_body: Mapping[str, Any]) -> list[dict[str, Any]]:
    return [
        message
        for message in messages_of(request_body)
        if message.get("role") == "user"
    ]


def assert_no_base64_in_history(session: Session, data: bytes) -> None:
    """history.jsonl 只存引用、绝不落 base64（逐字面对账）。"""
    raw = session.history.path.read_text(encoding="utf-8")
    assert base64.b64encode(data).decode() not in raw, "history 里落进了 base64 字节"
    assert "image_url" not in raw and "data:image" not in raw, raw[-400:]


async def send_with_media(
    session: Session,
    text: str,
    refs: Sequence[Mapping[str, Any]] | None = None,
    *,
    within: float = 30.0,
) -> dict[str, Any]:
    """发一条（可携带媒体引用的）消息并等到本轮 ``turn_result``。"""
    await session.send(text, media=refs)
    event = await session.watch.expect(["turn_result", "error"], within=within)
    assert event.type == "turn_result", event.data
    assert event.data["subtype"] == "success", event.data
    return event.data


# ── 场景 1：端到端（请求体 / history / 存储三层取证） ───────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_user_pasted_image_reaches_request_and_history(probe: Probe) -> None:
    """贴图消息进入当次模型调用：user content = [文本, image]，字节逐字节相同。"""
    data = png(3, 2, tint=0x44)
    probe.register(VISION_MODEL, Turn.of(text="I can see it"))
    session = await probe.session(model=VISION_MODEL)
    ref = seed_media(probe, session.session_id, data, name="pasted.png")

    result = await send_with_media(session, "look at this", [ref])
    assert result["result"] == "I can see it", result
    session.watch.assert_never("error")

    # ① 请求体：user 消息 = [原文本 part, image part]；解码 == 对象字节。
    context = probe.context(VISION_MODEL, 0)
    users = user_messages(context.body)
    assert len(users) == 1, context.describe()
    parts = content_parts(users[0])
    assert [part["type"] for part in parts] == ["text", "image_url"], parts
    assert parts[0]["text"] == "look at this", parts
    assert image_bytes(parts[1]) == data

    # ② history：引用进链、绝不落 base64（字节只在会话媒体目录）。
    view = probe.history(session)
    records = [m for m in view.messages() if m["role"] == "user"]
    assert len(records) == 1, view.describe()
    assert records[0]["content"] == "look at this"
    assert records[0]["media"] == [ref], records[0]
    assert_no_base64_in_history(session, data)
    view.assert_chain_invariants()

    # ③ 存储：对象在会话目录内（引用可兑现）。
    path = media_object_path(probe, session.session_id, ref["id"])
    assert path.read_bytes() == data, path


# ── 场景 2：纯图消息（content 为空） ──────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_pure_image_message_is_accepted(probe: Probe) -> None:
    """纯图消息合法：只有一个 image part、无空 text part，accepted 照发。"""
    data = png(2, 2, tint=0x55)
    probe.register(VISION_MODEL, Turn.of(text="got the image"))
    session = await probe.session(model=VISION_MODEL)
    ref = seed_media(probe, session.session_id, data, name="only.png")

    request_id = await session.send("", media=[ref])
    accepted = await session.watch.expect("user_message_accepted", within=15)
    assert accepted.data["origin_request_id"] == request_id, accepted.data
    assert accepted.data["content"] == "", accepted.data
    result = await session.watch.expect("turn_result", within=30)
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "got the image", result.data

    context = probe.context(VISION_MODEL, 0)
    users = user_messages(context.body)
    assert len(users) == 1, context.describe()
    parts = content_parts(users[0])
    assert [part["type"] for part in parts] == ["image_url"], parts
    assert image_bytes(parts[0]) == data

    view = probe.history(session)
    records = [m for m in view.messages() if m["role"] == "user"]
    assert records[0]["content"] == "" and records[0]["media"] == [ref], records[0]

    # 只有一张图的首条消息不让会话在列表里消失：标题回退为短标记（N3）。
    listing = await probe.driver_required.http.request("GET", "/api/session/list")
    entry = next(
        (
            item
            for item in listing.get("sessions", [])
            if item.get("id") == session.session_id
        ),
        None,
    )
    assert entry is not None, listing
    assert entry["name"] == IMAGE_ONLY_TITLE, entry


# ── 场景 3：多条带图消息合并（消息序 × 消息内序） ──────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_queued_media_messages_merge_in_order(probe: Probe) -> None:
    """一轮 drain 的两条带图消息 → 一条 user 消息，media 顺序 = 消息序×消息内序。"""
    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "sleep 3"})]),
        Turn.of(text="round two"),
        Turn.of(text="round three"),
    )
    session = await probe.session(model=VISION_MODEL, yolo=True)
    d1, d2, d3 = png(2, 2, tint=0x10), png(2, 2, tint=0x11), png(2, 2, tint=0x12)
    r1 = seed_media(probe, session.session_id, d1, name="q1.png")
    r2 = seed_media(probe, session.session_id, d2, name="q2.png")
    r3 = seed_media(probe, session.session_id, d3, name="q3.png")

    await session.send("start a slow tool")
    await session.watch.expect("tool_call", within=15)
    # 工具执行期间两条**带图**消息：第一条阻断 steer，两条一起等下一轮。
    await session.send("first image message", media=[r1, r2])
    await session.send("second image message", media=[r3])

    first = await session.watch.expect("turn_result", within=30)
    assert first.data["subtype"] == "success", first.data
    round_one = probe.context(VISION_MODEL, 1)
    wire = json.dumps(round_one.body, ensure_ascii=False)
    assert "first image message" not in wire, round_one.describe()

    second = await session.watch.expect("turn_result", within=30)
    assert second.data["subtype"] == "success", second.data

    context = probe.context(VISION_MODEL, 2)
    merged = user_messages(context.body)[-1]
    parts = content_parts(merged)
    assert [part["type"] for part in parts] == [
        "text",
        "image_url",
        "image_url",
        "image_url",
    ], parts
    assert parts[0]["text"] == "first image message\nsecond image message", parts
    assert [image_bytes(part) for part in parts[1:]] == [d1, d2, d3]

    # 落盘链与请求体同源：合并后的消息也只有一条 user 记录（引用齐全）。
    view = probe.history(session)
    records = [m for m in view.messages() if m["role"] == "user"]
    assert [m["content"] for m in records] == [
        "start a slow tool",
        "first image message\nsecond image message",
    ], view.describe()
    assert records[-1]["media"] == [r1, r2, r3], records[-1]


# ── 场景 4：steer 阻断（D7 红线） ────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_steer_stops_at_media_message(probe: Probe) -> None:
    """`[纯文本A, 带图B, 纯文本C]`：只有 A 被 steer，B/C 下一轮完整保序进入。"""
    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "sleep 3"})]),
        Turn.of(text="round two"),
        Turn.of(text="round three"),
    )
    session = await probe.session(model=VISION_MODEL, yolo=True)
    data = png(2, 2, tint=0x20)
    ref = seed_media(probe, session.session_id, data, name="steer.png")

    start_id = await session.send("start a slow tool")
    await session.watch.expect("tool_call", within=15)
    steer_id = await session.send("steer me now")
    image_id = await session.send("image message", media=[ref])
    after_id = await session.send("trailing text")

    first = await session.watch.expect("turn_result", within=30)
    assert first.data["subtype"] == "success", first.data

    # 本轮（工具轮）：steer note 只有 A；B/C 都不在本轮请求里。
    round_one = probe.context(VISION_MODEL, 1)
    tool_message = [
        message for message in messages_of(round_one.body) if message["role"] == "tool"
    ][0]
    # 工具结果可能被显式缓存模式数组化（末位文本块带 cache_control）——取 part 文本。
    note_part = content_parts(tool_message)[0]
    assert note_part["text"].startswith("[User steer note: steer me now]\n"), note_part
    round_one_wire = json.dumps(round_one.body, ensure_ascii=False)
    assert "image message" not in round_one_wire, round_one.describe()
    assert "trailing text" not in round_one_wire, round_one.describe()

    # 下一轮：带图消息与它后面的纯文本一起、按原序进入（一条合并消息）。
    second = await session.watch.expect("turn_result", within=30)
    assert second.data["subtype"] == "success", second.data

    context = probe.context(VISION_MODEL, 2)
    merged = user_messages(context.body)[-1]
    parts = content_parts(merged)
    assert [part["type"] for part in parts] == ["text", "image_url"], parts
    assert parts[0]["text"] == "image message\ntrailing text", parts
    assert image_bytes(parts[1]) == data

    # 消费确认的完整顺序与归属：A 随工具轮被消费，B/C 下一轮才被消费。
    accepted = session.watch.events("user_message_accepted")
    assert [
        (event.data["origin_request_id"], event.data["content"]) for event in accepted
    ] == [
        (start_id, "start a slow tool"),
        (steer_id, "steer me now"),
        (image_id, "image message"),
        (after_id, "trailing text"),
    ], [event.data for event in accepted]

    view = probe.history(session)
    assert [m["content"] for m in view.messages() if m["role"] == "user"] == [
        "start a slow tool",
        "image message\ntrailing text",
    ], view.describe()


# ── 场景 5：no_vision 占位附保存路径（D6 红线） ────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC, TEXT_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_no_vision_user_image_placeholder_carries_saved_path(
    probe: Probe,
) -> None:
    """text 模型收到贴图：无 image part，占位告知存在与路径；对象仍在。"""
    data = png(2, 2, tint=0x66)
    probe.register(TEXT_MODEL, Turn.of(text="text only"))
    session = await probe.session(model=TEXT_MODEL)
    ref = seed_media(probe, session.session_id, data, name="saved.png")

    await send_with_media(session, "look at this", [ref])

    context = probe.context(TEXT_MODEL, 0)
    users = user_messages(context.body)
    assert len(users) == 1, context.describe()
    parts = content_parts(users[0])
    assert [part["type"] for part in parts] == ["text", "text"], parts
    assert parts[0]["text"] == "look at this", "原文本块一字节不改"
    saved = str(media_object_path(probe, session.session_id, ref["id"]))
    assert parts[1]["text"] == NO_VISION_USER_PLACEHOLDER.format(path=saved), parts[1]
    assert "image_url" not in json.dumps(context.body, ensure_ascii=False)

    # 对象仍在存储里（媒体永不删除）。
    assert Path(saved).read_bytes() == data, saved


# ── 场景 6：memory 后端 → 降级文案（不报路径） ─────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC, TEXT_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_memory_backend_no_vision_placeholder_has_no_path(probe: Probe) -> None:
    """memory 后端没有本地文件：占位降级为"仍在会话中"，且引用的对象仍可寻址。"""
    data = png(2, 2, tint=0x77)
    write_image(probe, "mem.png", data)
    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "mem.png"})]),
        Turn.of(text="seen with vision"),
    )
    probe.register(TEXT_MODEL, Turn.of(text="text only"))
    session = await probe.session(
        model=VISION_MODEL, tools=["ReadImage"], backend="memory"
    )

    result = await session.chat("look at mem.png")
    assert result.data["subtype"] == "success", result.data
    call_event = session.watch.events(
        "tool_call_result", where={"tool_name": "ReadImage"}
    )[0]
    ref = call_event.data["tool_media"][0]
    assert ref["id"] == media_id(data), ref

    # 切到 text 模型（no_vision）后引用同一对象：memory 桶 per-session，仍可寻址。
    updated = await probe.driver_required.http.request(
        "POST",
        "/api/session/update",
        body={"session_id": session.session_id, "model_id": TEXT_MODEL},
    )
    assert updated.get("ok") is True, updated
    await send_with_media(session, "look again", [ref])

    context = probe.context(TEXT_MODEL, 0)
    merged = user_messages(context.body)[-1]
    parts = content_parts(merged)
    assert parts[0]["text"] == "look again", parts
    assert parts[1]["text"] == NO_VISION_USER_PLACEHOLDER_NO_PATH, parts
    wire = json.dumps(context.body, ensure_ascii=False)
    assert "image_url" not in wire
    assert "saved at" not in wire, "memory 后端不得出现本地路径"
    # tool 位（同一投影）仍是 tool 冻结文案——分流按消息角色，不串味。
    assert NO_VISION_TOOL_PLACEHOLDER in wire


# ── 场景 7：字节预算驱逐包含 user 图（D5 红线） ────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC], images=BUDGET_IMAGES)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_budget_eviction_includes_user_images(probe: Probe) -> None:
    """user 图参与请求期驱逐：最旧的 user 图被丢（user 变体占位），tool 图保留。"""
    data = png_padded(PADDED_PNG_BYTES, tint=0x88)
    tool_data = png_padded(PADDED_PNG_BYTES, tint=0x99)
    write_image(probe, "tool.png", tool_data)
    probe.register(
        VISION_MODEL,
        Turn.of(tool_calls=[ToolCall("ReadImage", {"path": "tool.png"})]),
        Turn.of(text="ok"),
    )
    session = await probe.session(model=VISION_MODEL, tools=["ReadImage"])
    ref = seed_media(probe, session.session_id, data, name="pasted.png")

    await session.send("look", media=[ref])
    result = await session.watch.expect("turn_result", within=30)
    assert result.data["subtype"] == "success", result.data

    # 下一轮请求（工具结果已在链上）：两张图合计 256 encoded > 预算 200
    # → 丢最旧一张 = user 图；tool 图仍以 image part 发射。
    context = probe.context(VISION_MODEL, 1)
    users = user_messages(context.body)
    parts = content_parts(users[0])
    assert [part["type"] for part in parts] == ["text", "text"], parts
    assert parts[0]["text"] == "look", "原文本块一字节不改"
    assert parts[1]["text"] == BUDGET_USER_PLACEHOLDER, parts

    emitted = [
        image_bytes(part)
        for message in messages_of(context.body)
        for part in image_parts(message)
    ]
    assert emitted == [tool_data], context.describe()
    wire = json.dumps(context.body, ensure_ascii=False)
    assert BUDGET_TOOL_PLACEHOLDER not in wire, context.describe()

    # 驱逐是请求期投影：链上引用与对象都不动。
    view = probe.history(session)
    records = [m for m in view.messages() if m["role"] == "user"]
    assert records[0]["media"] == [ref] and records[0]["content"] == "look", records[0]
    assert media_object_path(probe, session.session_id, ref["id"]).read_bytes() == data


# ── 场景 8：幽灵引用拒绝 ─────────────────────────────────────────


@pytest.mark.probe_env(models=[VISION_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_ghost_media_reference_is_rejected(probe: Probe) -> None:
    """不存在的 ref / 非法 id / 非法 mime：HTTP 400、WS 错误帧（可关联）；不进历史。"""
    probe.register(VISION_MODEL, Turn.of(text="should never run"))
    session = await probe.session(model=VISION_MODEL)
    ghost: dict[str, Any] = {
        "id": "b" * 64,
        "mime": "image/png",
        "bytes": 12,
        "width": 1,
        "height": 1,
        "name": "ghost.png",
    }
    http = probe.driver_required.http

    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "POST",
            "/api/session/send",
            body={
                "session_id": session.session_id,
                "content": "ghost",
                "media": [ghost],
            },
        )
    call = failure.value.call
    assert call.status == 400, call.render()
    detail = str((call.response or {}).get("detail", ""))
    assert "not found" in detail and ghost["id"][:8] in detail, detail

    malformed = dict(ghost, id="not-a-hash")
    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "POST",
            "/api/session/send",
            body={
                "session_id": session.session_id,
                "content": "bad id",
                "media": [malformed],
            },
        )
    assert failure.value.call.status == 400, failure.value.call.render()
    assert "invalid media reference" in str(
        (failure.value.call.response or {}).get("detail", "")
    )

    # 非法 mime 同样 400：mime 原样进线格式（data URL / media_type），写错会
    # 让整次请求被上游打死——入口挡下（对象存在也不放行）。
    bad_mime = dict(ghost, id=media_id(b"mime-rejected"), mime="image/svg+xml")
    with pytest.raises(DriverHttpError) as failure:
        await http.request(
            "POST",
            "/api/session/send",
            body={
                "session_id": session.session_id,
                "content": "bad mime",
                "media": [bad_mime],
            },
        )
    assert failure.value.call.status == 400, failure.value.call.render()
    assert "unsupported media reference mime" in str(
        (failure.value.call.response or {}).get("detail", "")
    )

    # WS 路径：错误帧（不是静默吞掉），且**可关联**回刚发出的那条消息——帧携带
    # 客户端自己的 request_id 与 session_id（前端据此走"拒绝 → 丢弃 pending /
    # 回填草稿"，而不是把它当成无主的服务端错误）。
    request_id = await session.send("ghost via ws", media=[ghost])
    error = await session.watch.expect("error", within=10)
    assert error.data["request_id"] == request_id, error.data
    assert error.data["session_id"] == session.session_id, error.data
    assert ghost["id"][:8] in str(error.data["message"]), error.data

    # 幽灵引用不进历史、也没有送达 ack / 消费确认。
    assert probe.history(session).messages() == [], probe.history(session).describe()
    session.watch.assert_never("delivered")
    session.watch.assert_never("user_message_accepted")
