"""SSE 编码器 —— 剧本 Turn → OpenAI 兼容 chunk 帧（纯函数，可独立对账）。

刻意不依赖 wing 侧的 SSE 实现：这是"外来生产者"的独立实现，帧形态按公开
协议（``data: {chunk}\\n\\n`` 帧 + ``data: [DONE]`` 终止；``choices[0].delta``
携带 ``content`` / ``reasoning_content`` / ``tool_calls[]`` 增量；末帧带
``usage``）写死，漂移以"场景全红"暴露。

分片与延迟：``Turn.chunk`` 决定 thinking / text 的分片粒度；``ToolCall.cut``
决定参数 JSON 的切断点；``Turn.delay`` 是**除首帧外**每帧前的等待秒数
（由 server 逐帧 sleep）。
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

from wing_probe.provider.script import Turn, Usage, resolve_call_id

SSE_CONTENT_TYPE = "text/event-stream; charset=utf-8"
DONE_TEXT = "data: [DONE]\n\n"


@dataclass(frozen=True)
class SSEFrame:
    """一个 SSE 帧：``data: {json}`` 或终止帧 ``data: [DONE]``。"""

    data: dict[str, Any] | None
    delay: float = 0.0

    @property
    def text(self) -> str:
        if self.data is None:
            return DONE_TEXT
        return f"data: {json.dumps(self.data, ensure_ascii=False)}\n\n"

    def encode(self) -> bytes:
        return self.text.encode("utf-8")


def done_frame() -> SSEFrame:
    """``data: [DONE]`` 终止帧。"""
    return SSEFrame(data=None)


def chunk_payload(
    *,
    completion_id: str,
    created: int,
    model: str,
    delta: dict[str, Any] | None = None,
    finish_reason: str | None = None,
    usage: Usage | None = None,
) -> dict[str, Any]:
    """一个 ``chat.completion.chunk`` 载荷。

    带 usage 的帧按协议给 ``choices: []``（usage 与选择无关）。
    """
    payload: dict[str, Any] = {
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [],
    }
    if usage is not None:
        payload["usage"] = usage.to_wire()
    else:
        payload["choices"] = [
            {"index": 0, "delta": dict(delta or {}), "finish_reason": finish_reason}
        ]
    return payload


def stream_frames(
    turn: Turn,
    *,
    turn_index: int,
    model: str,
    completion_id: str,
    created: int,
) -> list[SSEFrame]:
    """一轮的完整 SSE 帧序列（含 usage 末帧与 ``[DONE]``）。

    帧序：role 空帧 → thinking 分片 → text 分片 → tool_calls 增量分片
    → finish_reason 帧 → usage 帧 → ``[DONE]``。
    """
    payloads: list[dict[str, Any]] = [
        chunk_payload(
            completion_id=completion_id,
            created=created,
            model=model,
            delta={"role": "assistant", "content": ""},
        )
    ]
    for piece in turn.reasoning_chunks():
        payloads.append(
            chunk_payload(
                completion_id=completion_id,
                created=created,
                model=model,
                delta={"reasoning_content": piece},
            )
        )
    for piece in turn.content_chunks():
        payloads.append(
            chunk_payload(
                completion_id=completion_id,
                created=created,
                model=model,
                delta={"content": piece},
            )
        )
    for call_index, call in enumerate(turn.tool_calls):
        call_id = resolve_call_id(call, turn_index=turn_index, call_index=call_index)
        for piece_index, piece in enumerate(call.argument_chunks()):
            entry: dict[str, Any] = {
                "index": call_index,
                "function": {"arguments": piece},
            }
            if piece_index == 0:
                # 首片携带 id / type / name；后续片只有 index + arguments 增量。
                entry["id"] = call_id
                entry["type"] = "function"
                entry["function"]["name"] = call.name
            payloads.append(
                chunk_payload(
                    completion_id=completion_id,
                    created=created,
                    model=model,
                    delta={"tool_calls": [entry]},
                )
            )
    payloads.append(
        chunk_payload(
            completion_id=completion_id,
            created=created,
            model=model,
            delta={},
            finish_reason=turn.finish_reason,
        )
    )
    payloads.append(
        chunk_payload(
            completion_id=completion_id,
            created=created,
            model=model,
            usage=turn.resolved_usage,
        )
    )

    frames = [
        SSEFrame(data=payload, delay=0.0 if index == 0 else turn.delay)
        for index, payload in enumerate(payloads)
    ]
    frames.append(SSEFrame(data=None, delay=turn.delay))
    return frames


def encode_turn_stream(
    turn: Turn,
    *,
    turn_index: int,
    model: str,
    completion_id: str,
    created: int,
) -> bytes:
    """一轮的完整流式字节载荷（含 ``[DONE]``）——离线对账 / 自测用。"""
    return b"".join(
        frame.encode()
        for frame in stream_frames(
            turn,
            turn_index=turn_index,
            model=model,
            completion_id=completion_id,
            created=created,
        )
    )


def tool_calls_wire(turn: Turn, *, turn_index: int) -> list[dict[str, Any]]:
    """非流式 ``message.tool_calls``（完整参数——切断只影响流式分帧）。"""
    return [
        {
            "id": resolve_call_id(call, turn_index=turn_index, call_index=index),
            "type": "function",
            "function": {"name": call.name, "arguments": call.argument_text},
        }
        for index, call in enumerate(turn.tool_calls)
    ]


def completion_response(
    turn: Turn,
    *,
    turn_index: int,
    model: str,
    completion_id: str,
    created: int,
) -> dict[str, Any]:
    """非流式 ``chat.completion`` 响应（压缩调用走这条路）。"""
    message: dict[str, Any] = {"role": "assistant", "content": turn.text or ""}
    if turn.thinking is not None:
        message["reasoning_content"] = turn.thinking
    if turn.tool_calls:
        message["tool_calls"] = tool_calls_wire(turn, turn_index=turn_index)
    return {
        "id": completion_id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": message,
                "finish_reason": turn.finish_reason,
            }
        ],
        "usage": turn.resolved_usage.to_wire(),
    }


__all__ = [
    "DONE_TEXT",
    "SSE_CONTENT_TYPE",
    "SSEFrame",
    "chunk_payload",
    "completion_response",
    "done_frame",
    "encode_turn_stream",
    "stream_frames",
    "tool_calls_wire",
]
