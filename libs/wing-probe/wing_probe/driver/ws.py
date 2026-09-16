"""WS 订阅客户端 —— 传输层契约的独立实现（tasks 4.2，design D4）。

网关出网帧的上界是客户端单帧上限（16 MiB），超限载荷在唯一 wire 出口被切成
``_chunk`` 信封（``docs/dev/http-api.md``「单帧上界与分片」）。本客户端**独立复刻**
这条传输契约（不 import 任何 wing 侧实现）：

- ``max_size = 16 MiB``——与 TUI 客户端（tungstenite 默认值）对等；
- ``Reassembler``：纯状态机，按 ``type/_chunk, id, index, count, of_type, data`` 合并，
  三条不变量与 Rust 侧一致——**保序**（窗口打开期间其他帧缓冲不投递，闭合时先投完整
  事件、再按到达序放行缓冲帧）、**有界**（帧数 / 缓冲字节 / 不闭合超时）、**安全失败**
  （畸形信封绝不产出损坏事件，也绝不按 count 预分配内存）；
- 应用层只见**完整事件**：``on_event(session_id, type, data, Delivery)``；
- 原始帧（含信封帧）进 ``frames``（有界环形日志）；完整载荷进时间线（``Event.raw``）。

时间基准与 ``ProbeEnv.started_at`` 一致：``Delivery.at`` 是"相对 env 启动的秒数"，
与 ``Timeline`` 的时间戳同尺度（可直接互相比较）。
"""

from __future__ import annotations

import asyncio
import json
import re
import time
import uuid
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from websockets.asyncio.client import ClientConnection
from websockets.asyncio.client import connect as ws_connect
from websockets.exceptions import ConnectionClosed

from wing_probe.watch.timeline import FrameLog

#: 传输层保留的分片信封 type（应用事件 MUST NOT 使用 `_` 前缀）。
CHUNK_TYPE = "_chunk"

#: 客户端单帧上限（= tungstenite 默认 max_frame_size，网关按此保证上界）。
DEFAULT_MAX_SIZE = 16 * 1024 * 1024

#: 单事件帧数上限（防御畸形 ``count``：不预分配、不无界累积）。
MAX_CHUNKS = 1024

#: 重组窗口字节上限（未闭合分片 + 窗口内缓冲帧）。
MAX_BUFFERED_BYTES = 64 * 1024 * 1024

#: 分片不闭合超时（收到任一片即重置——静默才是异常信号）。
IDLE_TIMEOUT = 30.0

#: 畸形帧（不是合法 JSON 对象 / 没有 type）在时间线上的合成类型（网关永不发它）。
INVALID_FRAME_TYPE = "invalid_frame"

#: 信封探测的头部长度（``data`` 是最后一个字段，头部永远只有几十字节）。
_CHUNK_PROBE_CHARS = 128

#: 头部里的 type 判别（容忍空白差异；判定权威仍是 ``parse_envelope``）。
_CHUNK_TYPE_RE = re.compile(r'"type"\s*:\s*"_chunk"')

DEFAULT_OPEN_TIMEOUT = 10.0
DEFAULT_CLOSE_TIMEOUT = 5.0


class WsError(RuntimeError):
    """WS 连接 / 握手失败。"""


class ReassemblyError(WsError):
    """分片重组失败（畸形信封 / 序号空洞 / 缓冲越界 / 不闭合超时）。"""


@dataclass(frozen=True, slots=True)
class ChunkEnvelope:
    """``_chunk`` 信封（字段与语义见 ``docs/dev/http-api.md``）。"""

    id: str
    index: int
    count: int
    of_type: str
    data: str


@dataclass(frozen=True, slots=True)
class ChunkLimits:
    """重组防护限额（默认对齐 Rust 侧；测试可注入小值）。"""

    max_chunks: int = MAX_CHUNKS
    max_buffered_bytes: int = MAX_BUFFERED_BYTES
    idle_timeout: float = IDLE_TIMEOUT


DEFAULT_LIMITS = ChunkLimits()


@dataclass(frozen=True, slots=True)
class Delivery:
    """一条**完整**事件载荷（重组后的原始 JSON 文本 + 到达时刻）。"""

    text: str
    at: float
    """相对 env 启动的单调时钟（秒）。"""
    frames: int = 1
    """构成它的传输层帧数（``> 1`` 表示由 ``_chunk`` 信封重组而来）。"""


def is_chunk_frame(text: str) -> bool:
    """廉价探测：这一帧是否**可能**是 ``_chunk`` 信封（只看头部，不整帧解析）。

    只用于"要不要尝试按信封解析"的取舍与帧日志标注；判定权威在 ``parse_envelope``
    （它读 ``type`` 字段并校验其余字段）。误报无害：解析结果不是信封即按普通帧走。
    """
    return _CHUNK_TYPE_RE.search(text, 0, _CHUNK_PROBE_CHARS) is not None


def parse_envelope(text: str) -> ChunkEnvelope | None:
    """把一帧文本解析成信封；不是信封返回 ``None``，是信封但字段不合法则抛错。"""
    if not is_chunk_frame(text):
        return None
    try:
        data = json.loads(text)
    except ValueError:
        return None
    if not isinstance(data, dict) or data.get("type") != CHUNK_TYPE:
        return None
    chunk_id = data.get("id")
    index = data.get("index")
    count = data.get("count")
    of_type = data.get("of_type")
    payload = data.get("data")
    if (
        not isinstance(chunk_id, str)
        or isinstance(index, bool)
        or not isinstance(index, int)
        or isinstance(count, bool)
        or not isinstance(count, int)
        or not isinstance(payload, str)
    ):
        raise ReassemblyError(
            f"malformed chunk envelope: id={chunk_id!r} index={index!r} "
            f"count={count!r} of_type={of_type!r}"
        )
    return ChunkEnvelope(
        id=chunk_id,
        index=index,
        count=count,
        of_type=of_type if isinstance(of_type, str) else "",
        data=payload,
    )


@dataclass
class _Pending:
    """未闭合的重组窗口。"""

    id: str
    of_type: str
    count: int
    next_index: int
    parts: list[str]
    parts_bytes: int
    deadline: float


class Reassembler:
    """``_chunk`` 信封的重组状态机（纯逻辑、无 I/O——计时由调用方负责）。

    输入一帧文本，输出"可投递的完整事件载荷"或"仍在等后续分片"（``None``）；
    越界即 ``ReassemblyError``（调用方据此断开并报告，绝不产出半个事件）。
    """

    def __init__(self, limits: ChunkLimits = DEFAULT_LIMITS) -> None:
        self._limits = limits
        self._pending: _Pending | None = None
        self._buffered: deque[tuple[str, float]] = deque()
        self._buffered_bytes = 0

    @property
    def assembling(self) -> bool:
        """是否有未闭合的重组窗口。"""
        return self._pending is not None

    @property
    def pending_id(self) -> str | None:
        """未闭合窗口的事件 id（无窗口为 None）。"""
        return None if self._pending is None else self._pending.id

    def deadline(self) -> float | None:
        """ "必须收到下一片"的时刻（``time.monotonic`` 尺度）；无窗口为 None。"""
        return None if self._pending is None else self._pending.deadline

    def seconds_until_deadline(self, now: float) -> float | None:
        """距不闭合超时还有多少秒（无窗口为 None，已过期返回 0）。"""
        deadline = self.deadline()
        if deadline is None:
            return None
        return max(deadline - now, 0.0)

    def timeout_detail(self) -> str:
        """不闭合超时的报告文案。"""
        pending = self._pending
        if pending is None:
            return "no reassembly window was open"
        return (
            f"chunked event {pending.id!r} (of_type={pending.of_type!r}) stalled: "
            f"{pending.next_index}/{pending.count} fragment(s) received, no new frame "
            f"within {self._limits.idle_timeout:.1f}s"
        )

    def on_text(self, text: str, *, at: float) -> list[Delivery] | None:
        """处理一帧文本：返回可投递载荷（按到达序）或 ``None``（等待后续分片）。"""
        envelope = parse_envelope(text)
        if envelope is None:
            if self._pending is not None:
                self._buffer(text, at)
                return None
            return [Delivery(text=text, at=at)]
        self._accept(envelope, at=at)
        pending = self._pending
        if pending is None or pending.next_index < pending.count:
            return None
        return self._flush(at=at)

    # ── 内部 ──────────────────────────────────────────────────

    def _buffer(self, text: str, at: float) -> None:
        """窗口打开期间的其他帧：原样缓冲（保序），不解析、不投递。"""
        self._buffered.append((text, at))
        self._buffered_bytes += len(text)
        if self._total_bytes() > self._limits.max_buffered_bytes:
            raise ReassemblyError(
                f"chunk reassembly buffer exceeded {self._limits.max_buffered_bytes} "
                f"byte(s) while holding {self._buffered_bytes} buffered frame byte(s)"
            )

    def _total_bytes(self) -> int:
        pending_bytes = 0 if self._pending is None else self._pending.parts_bytes
        return pending_bytes + self._buffered_bytes

    def _accept(self, envelope: ChunkEnvelope, *, at: float) -> None:
        limits = self._limits
        if envelope.count < 2 or envelope.count > limits.max_chunks:
            raise ReassemblyError(
                f"chunk count {envelope.count} out of range (2..={limits.max_chunks}) "
                f"for id {envelope.id!r}"
            )
        pending = self._pending
        if pending is None:
            if envelope.index != 0:
                raise ReassemblyError(
                    f"chunk index {envelope.index} without a preceding index 0 "
                    f"for id {envelope.id!r}"
                )
            pending = _Pending(
                id=envelope.id,
                of_type=envelope.of_type,
                count=envelope.count,
                next_index=0,
                parts=[],
                parts_bytes=0,
                deadline=at + limits.idle_timeout,
            )
            self._pending = pending
        elif envelope.id != pending.id:
            raise ReassemblyError(
                f"chunk id changed mid-window: {pending.id!r} → {envelope.id!r} "
                f"(at index {envelope.index})"
            )
        elif envelope.index != pending.next_index:
            if envelope.index == pending.next_index - 1:
                raise ReassemblyError(
                    f"duplicate chunk index {envelope.index} for id {envelope.id!r}"
                )
            raise ReassemblyError(
                f"chunk index {envelope.index} out of sequence for id "
                f"{envelope.id!r} (expected {pending.next_index})"
            )
        pending.parts.append(envelope.data)
        pending.parts_bytes += len(envelope.data)
        pending.next_index += 1
        pending.deadline = at + limits.idle_timeout
        if self._total_bytes() > limits.max_buffered_bytes:
            raise ReassemblyError(
                f"chunk reassembly buffer exceeded {limits.max_buffered_bytes} byte(s) "
                f"while assembling {pending.count} fragment(s) of {pending.id!r}"
            )

    def _flush(self, *, at: float) -> list[Delivery]:
        """闭合窗口：先投递完整事件，再按到达序放行窗口内缓冲的帧。"""
        pending, self._pending = self._pending, None
        buffered, self._buffered = self._buffered, deque()
        self._buffered_bytes = 0
        deliveries: list[Delivery] = []
        if pending is not None:
            deliveries.append(
                Delivery(text="".join(pending.parts), at=at, frames=pending.count)
            )
        deliveries.extend(
            Delivery(text=text, at=frame_at) for text, frame_at in buffered
        )
        return deliveries


EventHandler = Callable[[str | None, str, dict[str, Any], Delivery], None]
"""事件回调：``(session_id, type, data, delivery)``。"""


class GatewayWS:
    """订阅 ``/ws`` 的连接：握手 → 读任务（重组 + 帧日志 + 回调）→ 上行请求。

    读任务是**唯一**的入站入口：每帧先进 ``frames``（原始帧保留），再经
    ``Reassembler`` 产出完整事件并回调（driver 据此路由到各 session 时间线）。
    读任务退出时记录原因（``close_reason``），消费方据此把"流死了"变成可诊断的
    失败，而不是静默空转。
    """

    def __init__(
        self,
        ws: ClientConnection,
        *,
        client_id: str,
        handler: EventHandler | None = None,
        frames: FrameLog | None = None,
        limits: ChunkLimits = DEFAULT_LIMITS,
        started_at: float | None = None,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._ws = ws
        self.client_id = client_id
        self._handler = handler
        self.frames = frames if frames is not None else FrameLog()
        self._reassembler = Reassembler(limits)
        self._clock = clock
        self._started_at = clock() if started_at is None else started_at
        self._closed = asyncio.Event()
        self._close_reason: str | None = None
        self._read_task: asyncio.Task[None] | None = None
        self.frames_received = 0
        self.events_received = 0
        self.sent_requests: list[dict[str, Any]] = []
        """上行帧留档（``ClientRequest`` 原样——便于核对 ask 的定向答复）。"""

    # ── 连接 ──────────────────────────────────────────────────

    @classmethod
    async def connect(
        cls,
        url: str,
        *,
        api_key: str | None = None,
        max_size: int = DEFAULT_MAX_SIZE,
        open_timeout: float = DEFAULT_OPEN_TIMEOUT,
        close_timeout: float = DEFAULT_CLOSE_TIMEOUT,
        limits: ChunkLimits = DEFAULT_LIMITS,
        handler: EventHandler | None = None,
        frames: FrameLog | None = None,
        started_at: float | None = None,
        clock: Callable[[], float] = time.monotonic,
    ) -> GatewayWS:
        """连接 ``/ws`` 并完成握手（首帧必须是 ``ConnectResponse``）。"""
        headers = {"Authorization": f"Bearer {api_key}"} if api_key else None
        try:
            ws = await ws_connect(
                url,
                max_size=max_size,
                open_timeout=open_timeout,
                close_timeout=close_timeout,
                additional_headers=headers,
                # probe 只连本地网关：绕开环境里的 HTTP(S)_PROXY（CI 常见污染源）
                proxy=None,
            )
        except Exception as exc:
            raise WsError(f"failed to connect to {url}: {exc!r}") from exc
        try:
            first = await ws.recv()
            data = json.loads(first if isinstance(first, str) else first.decode())
            client_id = data["client_id"]
        except Exception as exc:
            await _quiet_close(ws)
            raise WsError(
                f"gateway did not send a ConnectResponse on {url}: {exc!r}"
            ) from exc
        client = cls(
            ws,
            client_id=str(client_id),
            handler=handler,
            frames=frames,
            limits=limits,
            started_at=started_at,
            clock=clock,
        )
        client._read_task = asyncio.create_task(client._read_loop())
        return client

    # ── 状态 ──────────────────────────────────────────────────

    @property
    def closed(self) -> bool:
        return self._closed.is_set()

    @property
    def close_reason(self) -> str | None:
        """读任务退出原因（仍在运行时为 None）。"""
        return self._close_reason

    @property
    def assembling(self) -> bool:
        """是否有未闭合的分片窗口（诊断用）。"""
        return self._reassembler.assembling

    def now(self) -> float:
        """当前时刻（相对 env 启动的秒数）。"""
        return self._clock() - self._started_at

    async def wait_closed(self, timeout: float = DEFAULT_CLOSE_TIMEOUT) -> bool:
        """等待读任务退出（返回是否在时限内退出）。"""
        try:
            await asyncio.wait_for(self._closed.wait(), timeout)
        except TimeoutError:
            return False
        return True

    # ── 上行 ──────────────────────────────────────────────────

    async def send_request(
        self,
        session_id: str,
        content: str,
        *,
        tool_call_id: str | None = None,
        request_id: str | None = None,
    ) -> str:
        """发一个 ``ClientRequest`` 帧（返回 request_id）。"""
        if self.closed:
            raise WsError(
                f"cannot send on a closed connection ({self._close_reason or 'closed'})"
            )
        frame: dict[str, Any] = {
            "request_id": request_id or uuid.uuid4().hex,
            "session_id": session_id,
            "content": content,
        }
        if tool_call_id is not None:
            frame["tool_call_id"] = tool_call_id
        await self._ws.send(json.dumps(frame, ensure_ascii=False))
        self.sent_requests.append(frame)
        return str(frame["request_id"])

    async def close(self, reason: str = "closed by client") -> None:
        """关闭连接：记录原因 → 取消读任务 → 关 socket（幂等，不抛）。"""
        self._finish(reason)
        task, self._read_task = self._read_task, None
        if task is not None:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        await _quiet_close(self._ws)

    # ── 读任务 ────────────────────────────────────────────────

    async def _read_loop(self) -> None:
        try:
            while True:
                text = await self._recv_frame()
                at = self.now()
                self.frames_received += 1
                self.frames.add(text, at=at, chunk=is_chunk_frame(text))
                deliveries = self._reassembler.on_text(text, at=at)
                if deliveries is None:
                    continue
                for delivery in deliveries:
                    self._dispatch(delivery)
        except ReassemblyError as exc:
            self._finish(f"chunk reassembly failed: {exc}")
        except ConnectionClosed as exc:
            self._finish(f"connection closed by gateway: {exc}")
        except asyncio.CancelledError:
            self._finish(self._close_reason or "closed by client")
            raise
        except Exception as exc:  # pragma: no cover - 平台相关的读错误
            self._finish(f"read error: {exc!r}")

    async def _recv_frame(self) -> str:
        """读一帧；分片窗口打开时按不闭合超时设上界（静默即失败）。"""
        timeout = self._reassembler.seconds_until_deadline(self._clock())
        try:
            if timeout is None:
                raw = await self._ws.recv()
            else:
                raw = await asyncio.wait_for(self._ws.recv(), timeout)
        except TimeoutError as exc:
            raise ReassemblyError(self._reassembler.timeout_detail()) from exc
        if isinstance(raw, str):
            return raw
        return raw.decode("utf-8", "replace")

    def _dispatch(self, delivery: Delivery) -> None:
        """完整事件 → 回调（非法 JSON / 缺 type 记成合成类型，不静默丢帧）。"""
        self.events_received += 1
        try:
            data = json.loads(delivery.text)
        except ValueError as exc:
            data = None
            detail: str | None = f"frame is not valid JSON: {exc}"
        else:
            detail = None
        if not isinstance(data, dict):
            self._notify(
                None,
                INVALID_FRAME_TYPE,
                {
                    "error": detail or "frame payload is not a JSON object",
                    "raw": delivery.text[:512],
                },
                delivery,
            )
            return
        event_type = data.get("type")
        if not isinstance(event_type, str) or not event_type:
            self._notify(
                None,
                INVALID_FRAME_TYPE,
                {"error": "event has no 'type' field", "raw": delivery.text[:512]},
                delivery,
            )
            return
        session_id = data.get("session_id")
        self._notify(
            session_id if isinstance(session_id, str) else None,
            event_type,
            data,
            delivery,
        )

    def _notify(
        self,
        session_id: str | None,
        event_type: str,
        data: dict[str, Any],
        delivery: Delivery,
    ) -> None:
        if self._handler is None:
            return
        self._handler(session_id, event_type, data, delivery)

    def _finish(self, reason: str) -> None:
        if self._close_reason is None:
            self._close_reason = reason
        self._closed.set()


async def _quiet_close(ws: ClientConnection) -> None:
    try:
        await ws.close()
    except Exception:  # pragma: no cover - 关闭失败无需补救
        pass
