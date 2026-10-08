"""大帧切分端到端场景：> 8 MiB 出站事件 → `_chunk` 信封 → 客户端逐字节还原。

背景（`docs/dev/http-api.md`「单帧上界与分片」）：客户端单帧上限是 16 MiB，而事件载荷
可以更大（`sync_session` 实测 22 MB）。上界由**发送方**保证——超过软上限（8 MiB）的
序列化载荷在唯一 wire 出口（`GatewayServer._on_event` → `_send_frames`）按 UTF-8 边界
切成 N 帧信封，客户端读任务内合并还原，应用层零感知。

本场景用最省力形态构造 > 8 MiB 的**真实事件**：剧本让模型吐一段约 8.5 MiB 的 assistant
文本。`assistant_turn`（携带全文 content_blocks）与 `turn_result`（`result` 字段）的
序列化载荷都超过软上限，必须真的走切分分支。断言锚在两面：

- **应用层**：重组后的 `assistant_turn` 事件 `frames > 1`，文本与剧本**完全相等**
  （含一小段 CJK——非 ASCII 经 `\\uXXXX` 转义入帧后仍逐字符还原）；
- **原始帧层**：同一事件的 `_chunk` 信封同 id、index 连续、count 自洽，`data` 按序
  拼接 == 事件的原始载荷（逐字节）；每个信封帧 ≤ 8 MiB 软上限；帧日志无丢帧。

为什么能抓住回归：把切分关掉（`build_frames` 直通）时载荷仍在 16 MiB 以下——客户端
不会死，但 `frames == 1`、信封为零，第一条断言立即红。这正是"静默退化"（协议上限被
绕过但没人发现）的可观测面。
"""

from __future__ import annotations

import pytest

from wing_probe import ChunkEnvelope, Frame, Probe, Turn, parse_envelope

MODEL = "probe/frame-chunking"

#: 网关侧的软上限（`gateway/frames.py::SOFT_LIMIT_BYTES`）——线上契约，文档化。
SOFT_LIMIT_BYTES = 8 * 1024 * 1024

#: 目标文本体量（> 软上限：触发切分，又 < 16 MiB 客户端上限：直通时客户端不会死）。
TEXT_TARGET_BYTES = 8 * 1024 * 1024 + 512 * 1024

#: ASCII 周期块——主体必须是纯 ASCII：`GatewayServer._on_event` 用
#: `json.dumps(wire_dump(event))`（默认 `ensure_ascii=True`）序列化，非 ASCII 会
#: 变成 `\uXXXX`（6 字节/字符），帧字节数被放大；而帧日志上限 32 MiB。
BLOCK = "abcdefghijklmnop"

#: CJK 段（体量压小）：证明非 ASCII 内容经转义 + 分片重组后不丢不坏；同时避免
#: 把帧日志推过 32 MiB 上限（本场景总帧字节 ≈ 3 × 8.9 MiB）。
CJK_SEGMENT = "帧" * 1024

#: 尾部哨兵：即使逐字符相等断言被改弱，也能证明"最后一段没被截掉"。
TAIL_MARKER = "\n<probe-frame-chunking-end/>"

#: 剧本全文（确定性构造：块重复到目标体量，再补 CJK 段与哨兵）。
LONG_TEXT = BLOCK * (TEXT_TARGET_BYTES // len(BLOCK)) + CJK_SEGMENT + TAIL_MARKER

#: SSE 分片粒度（字符数）——把 8.5 MiB 控制成约百个 delta 帧，而不是一行 8.5 MiB。
SSE_CHUNK_CHARS = 64 * 1024

#: 帧日志上限是 32 MiB（`watch/timeline.py`）；本场景总帧字节 ≈ 3 × 8.9 MiB，确定不丢帧。
FRAME_LOG_BYTES = 32 * 1024 * 1024


def _assistant_turn_frames(probe: Probe) -> list[tuple[Frame, ChunkEnvelope]]:
    """原始帧日志里属于 `assistant_turn` 的信封帧（帧对象 + 解析出的信封）。"""
    found: list[tuple[Frame, ChunkEnvelope]] = []
    for frame in probe.frames:
        if not frame.chunk:
            continue
        envelope = parse_envelope(frame.text)
        if envelope is not None and envelope.of_type == "assistant_turn":
            found.append((frame, envelope))
    return found


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(180)
@pytest.mark.asyncio
async def test_oversized_event_is_chunked_and_reassembled(probe: Probe) -> None:
    """8.5 MiB assistant_turn → `_chunk` 多帧 → 客户端重组后与剧本逐字节一致。"""
    text_bytes = len(LONG_TEXT.encode("utf-8"))
    assert text_bytes > SOFT_LIMIT_BYTES, (
        f"fixture 必须先越过软上限（{text_bytes} <= {SOFT_LIMIT_BYTES}）"
    )

    probe.register(MODEL, Turn.of(text=LONG_TEXT, chunk=SSE_CHUNK_CHARS))
    session = await probe.session(model=MODEL)

    result = await session.chat("emit the long text", within=90)
    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    # ── 应用层：重组后的事件 ────────────────────────────────
    turns = session.watch.events(type="assistant_turn")
    assert len(turns) == 1, [event.type for event in session.watch.events()]
    event = turns[0]
    assert event.frames > 1, (
        f"载荷 {len(event.raw.encode('utf-8'))} 字节仍单帧到达——切分分支没有被走到"
    )
    assert len(event.raw.encode("utf-8")) > SOFT_LIMIT_BYTES, len(event.raw)
    blocks = event.data["content_blocks"]
    texts = [block["text"] for block in blocks if block["type"] == "text"]
    assert len(texts) == 1, blocks
    assert texts[0] == LONG_TEXT, (
        f"重组后的文本必须与剧本逐字符一致（长度 {len(texts[0])} vs {len(LONG_TEXT)}）"
    )
    assert texts[0].endswith(TAIL_MARKER), texts[0][-80:]

    # ── 原始帧层：`_chunk` 信封自洽 + 逐字节拼接 ─────────────
    frames = _assistant_turn_frames(probe)
    assert len(frames) >= 2, (
        f"原始帧日志里 assistant_turn 的信封帧只有 {len(frames)} 条"
    )
    assert probe.driver_required.frames.dropped == 0, (
        "帧日志发生丢弃（容量上限被突破）——信封断言的前提不成立；"
        f"总上限 {FRAME_LOG_BYTES} bytes / 512 frames"
    )
    envelopes = [envelope for _, envelope in frames]
    count = envelopes[0].count
    assert count == len(envelopes), (count, len(envelopes))
    assert event.frames == count, (event.frames, count)
    assert len({envelope.id for envelope in envelopes}) == 1, envelopes
    assert [envelope.index for envelope in envelopes] == list(range(count)), envelopes
    assert "".join(envelope.data for envelope in envelopes) == event.raw, (
        "信封 data 按序拼接必须与事件的原始载荷逐字节相等"
    )
    # 每个信封帧（含信封与转义开销）≤ 软上限——UTF-8 字节数才是 wire 口径。
    oversized = [
        (envelope.index, len(frame.text.encode("utf-8")))
        for frame, envelope in frames
        if len(frame.text.encode("utf-8")) > SOFT_LIMIT_BYTES
    ]
    assert oversized == [], oversized

    # ── 落盘：全文进入 assistant 消息（重组的另一份独立证据） ──
    assistant = [
        message
        for message in probe.history(session).messages()
        if message["role"] == "assistant"
    ]
    assert len(assistant) == 1, probe.history(session).messages()
    assert assistant[0]["content"] == LONG_TEXT, len(assistant[0]["content"])
