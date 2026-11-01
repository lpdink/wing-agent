"""发射时刻账本：假 Provider 每帧交给传输层之前的 ``time.monotonic()``。

被钉住的语义：``frame_index`` 只数 JSON 载荷帧（``[DONE]`` 不计）且与客户端读到的
SSE 事件序一一对应；``at`` 取在写之前、写失败的那帧不进账；``request_index`` 在多请求
并发时各归各（= ``RequestLog`` 留档的全局 ``index``）。全部在进程内跑，不起网关。
"""

from __future__ import annotations

import asyncio
import json
import time

import httpx
import pytest
from _fake_provider import (
    COMPLETION_ID,
    CREATED,
    request_body,
    running_provider,
)
from aiohttp import web

from wing_probe.provider.script import Script, ToolCall, Turn, Usage
from wing_probe.provider.server import FakeProvider, FrameEmit
from wing_probe.provider.sse import FRAME_KINDS, SSEFrame, stream_frames

MODEL = "probe/emits"
DONE_LINE = "data: [DONE]"


class RecordingResponse(web.StreamResponse):
    """只记写入顺序与完成时刻的响应替身（``_write_frames`` 只碰 ``write``）。"""

    def __init__(self, fail_after: int | None = None) -> None:
        super().__init__()
        self.chunks: list[bytes] = []
        self.written_at: list[float] = []
        self._fail_after = fail_after

    async def write(self, data: bytes | bytearray | memoryview) -> None:
        if self._fail_after is not None and len(self.chunks) >= self._fail_after:
            raise ConnectionResetError("client went away")
        self.chunks.append(bytes(data))
        self.written_at.append(time.monotonic())


def sse_frames(turn: Turn, *, turn_index: int = 0) -> list[SSEFrame]:
    return stream_frames(
        turn,
        turn_index=turn_index,
        model=MODEL,
        completion_id=COMPLETION_ID,
        created=CREATED,
    )


def payload_count(turn: Turn, *, turn_index: int = 0) -> int:
    """该轮写出的 JSON 载荷帧数（``[DONE]`` 不计）。"""
    return sum(
        1 for frame in sse_frames(turn, turn_index=turn_index) if frame.data is not None
    )


def without_created(payload: dict) -> dict:
    """去掉服务端墙钟字段 ``created``（两侧取的不是同一个数）。"""
    return {key: value for key, value in payload.items() if key != "created"}


async def sse_payload_lines(
    provider: FakeProvider,
    *,
    model: str = MODEL,
    close_after: int | None = None,
    **overrides: object,
) -> list[str]:
    """流式请求，返回客户端读到的 ``data: {json}`` 行（``[DONE]`` 剔除）。"""
    lines: list[str] = []
    async with httpx.AsyncClient(
        base_url=provider.base_url, timeout=10.0, trust_env=False
    ) as client:
        async with client.stream(
            "POST", "/chat/completions", json=request_body(model, **overrides)
        ) as response:
            assert response.status_code == 200, response.status_code
            async for line in response.aiter_lines():
                if not line.startswith("data: "):
                    continue
                if line == DONE_LINE:
                    break
                lines.append(line[len("data: ") :])
                if close_after is not None and len(lines) >= close_after:
                    break
    return lines


# ── 帧序对账 ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_emit_count_matches_the_frames_the_client_reads() -> None:
    """条数与逐帧内容都和客户端可观测的载荷帧一一对应（``[DONE]`` 两侧都剔）。"""
    turn = Turn.of(
        thinking="想一想",
        text="abcdefgh",
        chunk=2,
        tool_calls=[ToolCall("Bash", {"command": "ls"}, cut=4)],
        usage=Usage(prompt_tokens=7, completion_tokens=3),
    )
    async with running_provider() as provider:
        provider.register(MODEL, Script(turn))

        observed = await sse_payload_lines(provider)

        emits = provider.emits_for(0)
        assert len(observed) == payload_count(turn) == len(emits)
        # 序号逐帧对上：第 i 条记录就是第 i 个载荷帧（0 = role 空帧）。
        expected = [
            without_created(json.loads(frame.text[len("data: ") :]))
            for frame in sse_frames(turn)
            if frame.data is not None
        ]
        got = [without_created(json.loads(line)) for line in observed]
        assert got == expected
        assert [emit.frame_index for emit in provider.frame_emits] == list(
            range(len(emits))
        )
        # 同一条流内时刻不减。
        assert emits == sorted(emits)


@pytest.mark.asyncio
async def test_emit_kinds_mark_the_delta_channel() -> None:
    """``kind`` 逐帧标出通道；只有 content / thinking / tool_call 帧能与客户端事件配对。"""
    turn = Turn.of(
        thinking="想一想",
        text="abcd",
        chunk=2,
        tool_calls=[ToolCall("Bash", {"command": "ls"}, cut=4)],
    )
    async with running_provider() as provider:
        provider.register(MODEL, Script(turn))

        await sse_payload_lines(provider)

        kinds = [emit.kind for emit in provider.frame_emits]
        assert kinds == [
            "meta",  # role 空帧
            "thinking",
            "thinking",
            "content",
            "content",
            "tool_call",
            "tool_call",
            "meta",  # finish_reason 帧
            "meta",  # usage 帧
        ]
        assert set(kinds) <= FRAME_KINDS
        # 下游配对前提：content 帧数 == 剧本文本的切片数（chunk=1 时即文本长度）。
        assert kinds.count("content") == len(turn.content_chunks()) == 2


@pytest.mark.asyncio
async def test_meta_frames_have_no_delta_channel() -> None:
    """``meta`` 覆盖 role / finish / usage 三类帧：这三种帧的载荷里没有 delta 增量。"""
    turn = Turn.of(
        text="ab", chunk=1, usage=Usage(prompt_tokens=5, completion_tokens=2)
    )
    async with running_provider() as provider:
        provider.register(MODEL, Script(turn))
        await sse_payload_lines(provider)

        meta = [emit for emit in provider.frame_emits if emit.kind == "meta"]
        assert len(meta) == 3, [emit.kind for emit in provider.frame_emits]
        payloads = [frame.data for frame in sse_frames(turn) if frame.data is not None]
        meta_payloads = [payloads[emit.frame_index] for emit in meta]
        assert meta_payloads[0]["choices"][0]["delta"] == {
            "role": "assistant",
            "content": "",
        }
        assert meta_payloads[1]["choices"][0]["finish_reason"] == "stop"
        assert meta_payloads[2]["choices"] == [] and "usage" in meta_payloads[2]


@pytest.mark.asyncio
async def test_truncated_turn_counts_only_payload_frames() -> None:
    """没有 finish 帧的轮（上游截断）同样只数载荷帧，``[DONE]`` 不占位。"""
    async with running_provider() as provider:
        truncated = Turn.of(text="cut off", truncated=True)
        provider.register(MODEL, Script(truncated))
        await sse_payload_lines(provider)

        assert len(provider.emits_for(0)) == payload_count(truncated)
        assert provider.requests.get().stream is True


@pytest.mark.asyncio
async def test_emit_timestamp_is_taken_before_the_write() -> None:
    """``at`` 早于该帧写完的时刻（度量「生产者最早能交出去」而非写耗时）。"""
    provider = FakeProvider()
    turn = Turn.of(text="abcd", chunk=1)
    response = RecordingResponse()

    await provider._write_frames(response, sse_frames(turn), request_index=0)

    payloads = payload_count(turn)
    assert len(provider.frame_emits) == payloads
    assert len(response.written_at) == payloads + 1, "写入含 [DONE]"
    assert [emit.frame_index for emit in provider.frame_emits] == list(range(payloads))
    assert all(
        emit.at <= written_at
        for emit, written_at in zip(provider.frame_emits, response.written_at)
    )


@pytest.mark.asyncio
async def test_frame_whose_write_fails_is_not_recorded() -> None:
    """客户端中途断开：账本只剩已写成的帧（前缀），异常照常向上抛。"""
    provider = FakeProvider()
    turn = Turn.of(text="abcdefgh", chunk=1)
    response = RecordingResponse(fail_after=3)

    with pytest.raises(ConnectionResetError):
        await provider._write_frames(response, sse_frames(turn), request_index=0)

    assert [emit.frame_index for emit in provider.frame_emits] == [0, 1, 2]


@pytest.mark.asyncio
async def test_client_leaving_early_caps_the_ledger_at_the_transport_buffers() -> None:
    """真连接上的早退：账本停在「本地传输层接受了的帧」处，序号仍是 0..k-1。

    记的是**上限**而不是客户端断开的那一刻：写了就进内核缓冲的帧仍算数（``delay=0``
    的突发里账本会记满，而客户端只读到几帧）。
    """
    turn = Turn.of(text="abcdefgh", chunk=1, delay=0.02)
    async with running_provider() as provider:
        provider.register(MODEL, Script(turn))

        await sse_payload_lines(provider, close_after=2)

        recorded = [emit.frame_index for emit in provider.frame_emits]
        assert recorded == list(range(len(recorded))), recorded
        assert len(recorded) >= 2, "客户端读到的两帧必须都有时刻"
        assert len(recorded) <= payload_count(turn)


# ── 多请求归属 ──────────────────────────────────────────────


@pytest.mark.asyncio
async def test_concurrent_requests_keep_their_own_frames() -> None:
    """两个 model 交错发射：``request_index`` 归属不串，条数各归各。"""
    short = Turn.of(text="b", first_delay=0.15, delay=0.01)
    long = Turn.of(text="abcdef", chunk=1, first_delay=0.15, delay=0.01)
    async with running_provider() as provider:
        provider.register("probe/emits-short", Script(short))
        provider.register("probe/emits-long", Script(long))

        await asyncio.gather(
            sse_payload_lines(provider, model="probe/emits-short"),
            sse_payload_lines(provider, model="probe/emits-long"),
        )

        indices = {entry.model: entry.index for entry in provider.requests}
        short_emits = provider.emits_for(indices["probe/emits-short"])
        long_emits = provider.emits_for(indices["probe/emits-long"])

        assert len(short_emits) == payload_count(short)
        assert len(long_emits) == payload_count(long)
        assert len(short_emits) != len(long_emits), "两个剧本帧数不同，串了会立刻可见"
        assert len(provider.frame_emits) == len(short_emits) + len(long_emits)
        for index in indices.values():
            own = [
                e.frame_index for e in provider.frame_emits if e.request_index == index
            ]
            assert own == list(range(len(own)))


# ── 空表、越界与账本生命周期 ────────────────────────────────


@pytest.mark.asyncio
async def test_non_streaming_request_records_no_frames() -> None:
    """压缩调用走非流式：没有帧，``emits_for`` 是空列表（不是「下标错」）。"""
    async with running_provider() as provider:
        provider.register(MODEL, Script(Turn.of(text="<summary>ok</summary>")))
        async with httpx.AsyncClient(
            base_url=provider.base_url, timeout=10.0, trust_env=False
        ) as client:
            response = await client.post(
                "/chat/completions", json=request_body(MODEL, stream=False)
            )
            assert response.status_code == 200

        assert provider.emits_for(0) == []
        assert provider.frame_emits == ()


@pytest.mark.asyncio
async def test_streaming_request_that_fails_before_any_frame_has_no_frames() -> None:
    """流式请求撞上未注册模型（500）：留档有一条，帧账本一条都没有。"""
    async with running_provider() as provider:
        async with httpx.AsyncClient(
            base_url=provider.base_url, timeout=10.0, trust_env=False
        ) as client:
            response = await client.post(
                "/chat/completions", json=request_body("probe/unregistered")
            )
            assert response.status_code == 500

        assert provider.emits_for(0) == []
        assert provider.frame_emits == ()


def test_emits_for_rejects_an_unknown_request_index() -> None:
    """从未留档的下标抛 ``IndexError``（打错下标不该得到一张空延迟表）。"""
    provider = FakeProvider()
    for index in (0, 1, -1):
        with pytest.raises(IndexError, match="out of range"):
            provider.emits_for(index)

    provider.requests.record({"model": MODEL}, model=MODEL)
    assert provider.emits_for(0) == []
    with pytest.raises(IndexError, match="out of range"):
        provider.emits_for(1)


@pytest.mark.asyncio
async def test_emits_for_returns_a_copy_of_the_ledger() -> None:
    """返回副本：调用方改这张表不会影响账本（"只读暴露"的兑现）。"""
    provider = FakeProvider()
    await provider._write_frames(
        RecordingResponse(), sse_frames(Turn.of(text="ab")), request_index=0
    )
    provider.requests.record({"model": MODEL}, model=MODEL)

    returned = provider.emits_for(0)
    returned.append(-1.0)
    returned[0] = -1.0

    assert provider.emits_for(0) == [emit.at for emit in provider.frame_emits]


@pytest.mark.asyncio
async def test_emits_for_refuses_a_reissued_request_index() -> None:
    """留档被清过 → 序号从 0 重新发号：旧帧与新请求无法区分，宁可报错。"""
    async with running_provider() as provider:
        provider.register(MODEL, Script(Turn.of(text="ab"), Turn.of(text="cd")))
        await sse_payload_lines(provider)
        assert provider.emits_for(0)

        provider.requests.clear()

        await sse_payload_lines(provider)
        with pytest.raises(RuntimeError, match="indices are reissued"):
            provider.emits_for(0)
        with pytest.raises(RuntimeError, match="indices are reissued"):
            _ = provider.frame_emits


@pytest.mark.asyncio
async def test_frame_emits_is_a_read_only_snapshot() -> None:
    """``frame_emits`` 是只读快照：就地清 / 追加改不动账本（要干净账本就重建 provider）。"""
    provider = FakeProvider()
    await provider._write_frames(
        RecordingResponse(), sse_frames(Turn.of(text="ab")), request_index=0
    )

    snapshot = provider.frame_emits
    assert isinstance(snapshot, tuple)
    assert len(snapshot) == payload_count(Turn.of(text="ab"))
    with pytest.raises(AttributeError):
        snapshot.clear()  # ty: ignore[unresolved-attribute]

    assert len(provider.frame_emits) == len(snapshot)


def test_frame_emit_and_facade_exports() -> None:
    """记录类型经两级门面与模块 ``__all__`` 登记（同一个对象）。"""
    import wing_probe
    import wing_probe.provider
    import wing_probe.provider.server

    assert "FrameEmit" in wing_probe.provider.server.__all__
    assert wing_probe.FrameEmit is wing_probe.provider.FrameEmit
    assert wing_probe.FrameEmit is FrameEmit
