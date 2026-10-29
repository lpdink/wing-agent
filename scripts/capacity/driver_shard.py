#!/usr/bin/env python3
"""容量测量的 driver 分片（独立子进程，只经公开 WS / HTTP 驱动网关）。

一片负责若干会话：连 ``/ws`` 拿 ``client_id`` → 逐会话 ``/api/session/subscribe`` →
按形态给的 prompt 文本驱动轮次，记录**采样窗口内**的到达时刻。为什么要独立进程
见 ``README.md``「进程拓扑」：分片与假 Provider 同进程会互相抢事件循环，让客户端
饱和被误读成网关退化。

握手是 stdout / stdin 上的 JSON 行（窗口起点由 coordinator 在**所有分片都就绪后**
计算，避免慢机器上「窗口已开始但分片还没连上」）：

    → {"event": "ready", "shard": 0, "ws_connections": 2, "sessions": 8}
    ← {"window_start": 123.4, "window_end": 143.4}
    → {"event": "done", "turns": 40, "text_frames": 10000, "errors": 0}

样本走文件（``--config`` 里的 ``out``）：一档最大约 10^6 个到达时刻，走管道会让
coordinator 与分片在同一个窗口里互相同步。时间戳存「相对 window_start 的微秒」。
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import time
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from types import ModuleType
from typing import Any

import httpx
from wing_probe.driver.session import ws_url
from wing_probe.driver.ws import Delivery, GatewayWS
from wing_probe.watch.timeline import FrameLog

import proc
import shapes
from metrics import TurnSample

#: 每条 WS 连接保留的原始帧数（只够诊断：样本是到达时刻，不是帧内容）。
FRAME_TAIL = 64

#: 建立 WS 连接 + 订阅的并发上限（1000 条连接串行握手会把就绪时间拉成秒级）。
CONNECT_CONCURRENCY = 16

SUBSCRIBE_TIMEOUT = 30.0


class ShardError(RuntimeError):
    """分片自身失败（配置不对 / 连不上 / 订阅被拒）。"""


@dataclass(frozen=True)
class ShardConfig:
    """一片的输入（coordinator 每次 spawn 前写一份 JSON）。"""

    shard: int
    gateway_url: str
    sessions: tuple[str, ...]
    subscribers: str
    shape: str
    out: Path

    @classmethod
    def load(cls, path: str | Path) -> ShardConfig:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
        if not isinstance(raw, dict):
            raise ShardError(f"shard config must be a JSON object: {path}")
        sessions = raw.get("sessions")
        if not isinstance(sessions, list):
            raise ShardError(f"shard config has no sessions list: {path}")
        subscribers = raw.get("subscribers")
        if subscribers not in ("per-session", "per-shard"):
            raise ShardError(
                f"shard config subscribers must be per-session or per-shard, "
                f"got {subscribers!r}"
            )
        return cls(
            shard=int(raw["shard"]),
            gateway_url=str(raw["gateway_url"]),
            sessions=tuple(str(entry) for entry in sessions),
            subscribers=str(subscribers),
            shape=str(raw["shape"]),
            out=Path(str(raw["out"])),
        )


@dataclass
class _SessionState:
    """一个会话的驱动状态（同一时刻最多一轮在飞）。"""

    session_id: str
    ws: GatewayWS
    seq: int = 0
    prompt: str = ""
    sent_at: float | None = None
    arrivals: list[float] = field(default_factory=list)


class Shard:
    """一片的驱动 + 记录（事件回调只写紧凑结构，不建 Timeline）。"""

    def __init__(self, config: ShardConfig, shape: ModuleType) -> None:
        self.config = config
        self.shape = shape
        self.states: dict[str, _SessionState] = {}
        self.connections: list[GatewayWS] = []
        self.turns: list[TurnSample] = []
        self.text_frames = 0
        self.errors = 0
        self.error_details: list[str] = []
        self._pending: asyncio.Queue[str] = asyncio.Queue()
        self._window_start = 0.0
        self._window_end = 0.0
        self._cpu_start: float | None = None
        self._cpu_end: float | None = None
        self._closing = False

    # ── 连接 ────────────────────────────────────────────────

    async def connect(self) -> None:
        """建 WS 连接并订阅本片的全部会话（``per-session`` 一会话一条连接）。"""
        service = httpx.AsyncClient(
            base_url=self.config.gateway_url, timeout=SUBSCRIBE_TIMEOUT, trust_env=False
        )
        gate = asyncio.Semaphore(CONNECT_CONCURRENCY)

        async def attach(session_id: str, ws: GatewayWS) -> None:
            async with gate:
                response = await service.post(
                    "/api/session/subscribe",
                    json={"session_id": session_id},
                    headers={"X-Client-Id": ws.client_id},
                )
            if response.status_code != 200:
                raise ShardError(
                    f"subscribe {session_id} returned {response.status_code}: "
                    f"{response.text[:200]}"
                )
            self.states[session_id] = _SessionState(session_id=session_id, ws=ws)

        try:
            if self.config.subscribers == "per-shard":
                ws = await self._open()
                self.connections.append(ws)
                await asyncio.gather(
                    *(attach(session_id, ws) for session_id in self.config.sessions)
                )
            else:

                async def one(session_id: str) -> None:
                    async with gate:
                        ws = await self._open()
                    self.connections.append(ws)
                    await attach(session_id, ws)

                await asyncio.gather(*(one(sid) for sid in self.config.sessions))
        finally:
            await service.aclose()

    async def _open(self) -> GatewayWS:
        """开一条订阅连接（``started_at=0.0``：``Delivery.at`` 即绝对单调时刻）。"""
        return await GatewayWS.connect(
            ws_url(self.config.gateway_url),
            started_at=0.0,
            handler=self._on_event,
            frames=FrameLog(max_frames=FRAME_TAIL),
        )

    async def close(self) -> None:
        for ws in self.connections:
            await ws.close()

    # ── 窗口 ────────────────────────────────────────────────

    def open_window(self, window_start: float, window_end: float) -> None:
        """接收窗口边界并立刻开始驱动（warmup 从这一刻起算，样本只记窗口内）。"""
        self._window_start = window_start
        self._window_end = window_end
        self._cpu_start = proc.self_cpu_seconds()
        for session_id in self.states:
            self._pending.put_nowait(session_id)

    def close_window(self) -> None:
        """窗口收尾：停发新轮次，并把 CPU 记账钉在窗口末（之后的收尾不计入）。"""
        self._closing = True
        self._cpu_end = proc.self_cpu_seconds()

    def _on_event(
        self,
        session_id: str | None,
        event_type: str,
        data: dict[str, Any],
        delivery: Delivery,
    ) -> None:
        """WS 回调：只记到达时刻与轮次边界（读任务里同步执行，越轻越好）。"""
        state = self.states.get(session_id) if session_id else None
        if state is None:
            return
        now = delivery.at
        if event_type == "text":
            if (
                state.sent_at is not None
                and self._window_start <= now <= self._window_end
            ):
                state.arrivals.append(now)
                self.text_frames += 1
            return
        if event_type not in ("turn_result", "error"):
            return
        if event_type == "error" or data.get("is_error"):
            self.errors += 1
            if len(self.error_details) < 8:
                self.error_details.append(f"{event_type}: {str(data)[:200]}")
        sent_at, state.sent_at = state.sent_at, None
        arrivals, state.arrivals = state.arrivals, []
        if (
            sent_at is not None
            and sent_at >= self._window_start
            and now <= self._window_end
        ):
            self.turns.append(TurnSample(state.prompt, sent_at, now, tuple(arrivals)))
        if not self._closing and now < self._window_end:
            self._pending.put_nowait(state.session_id)

    async def pump(self) -> None:
        """轮次泵：一轮结束后立刻发下一条 prompt（窗口结束即停发）。"""
        while True:
            session_id = await self._pending.get()
            state = self.states[session_id]
            if time.monotonic() >= self._window_end:
                continue
            state.prompt = self.shape.prompt_text(session_id, state.seq)
            state.seq += 1
            state.arrivals = []
            state.sent_at = time.monotonic()
            try:
                await state.ws.send_request(session_id, state.prompt)
            except Exception as exc:  # noqa: BLE001 - 单条连接坏掉不该拖死整片
                self.errors += 1
                state.sent_at = None
                if len(self.error_details) < 8:
                    self.error_details.append(f"send failed for {session_id}: {exc!r}")

    # ── 产物 ────────────────────────────────────────────────

    def payload(self) -> dict[str, Any]:
        start = self._window_start

        def us(moment: float) -> int:
            return int(round((moment - start) * 1e6))

        return {
            "shard": self.config.shard,
            "gateway_url": self.config.gateway_url,
            "subscribers": self.config.subscribers,
            "shape": self.config.shape,
            "ws_connections": len(self.connections),
            "sessions": list(self.config.sessions),
            "frames_received": sum(ws.frames_received for ws in self.connections),
            "cpu_seconds": None
            if self._cpu_start is None or self._cpu_end is None
            else round(self._cpu_end - self._cpu_start, 6),
            "text_frames": self.text_frames,
            "errors": self.errors,
            "error_details": self.error_details,
            "window": {"start": start, "end": self._window_end},
            "turns": [
                {
                    "prompt": turn.prompt,
                    "sent_us": us(turn.sent_at),
                    "result_us": us(turn.result_at),
                    "arrivals_us": [us(moment) for moment in turn.arrivals],
                }
                for turn in self.turns
            ],
        }

    def write(self) -> None:
        self.config.out.parent.mkdir(parents=True, exist_ok=True)
        text = json.dumps(self.payload(), ensure_ascii=False)
        self.config.out.write_text(text, encoding="utf-8")


def emit(event: str, **fields: Any) -> None:
    """向 coordinator 报一行 JSON（stdout 只有握手行，样本走文件）。"""
    sys.stdout.write(json.dumps({"event": event, **fields}, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def read_window() -> tuple[float, float]:
    """阻塞读窗口边界（coordinator 在所有分片就绪后下发；EOF 即失败）。"""
    line = sys.stdin.readline()
    if not line:
        raise ShardError("coordinator closed stdin before sending the window")
    raw = json.loads(line)
    if not isinstance(raw, dict):
        raise ShardError(f"window message must be a JSON object: {line.strip()!r}")
    return float(raw["window_start"]), float(raw["window_end"])


async def run(config_path: Path) -> int:
    config = ShardConfig.load(config_path)
    shape = shapes.resolve(config.shape)
    shard = Shard(config, shape)
    try:
        await shard.connect()
    except Exception as exc:  # noqa: BLE001 - 连不上就报错退出，让 coordinator 响
        emit("error", shard=config.shard, message=f"connect failed: {exc!r}")
        return 1
    emit(
        "ready",
        shard=config.shard,
        ws_connections=len(shard.connections),
        sessions=len(shard.states),
    )
    try:
        window_start, window_end = await asyncio.to_thread(read_window)
    except Exception as exc:  # noqa: BLE001 - 同上
        emit("error", shard=config.shard, message=f"window handshake failed: {exc!r}")
        await shard.close()
        return 1

    shard.open_window(window_start, window_end)
    pump = asyncio.create_task(shard.pump())
    try:
        await asyncio.sleep(max(0.0, window_end - time.monotonic()))
    finally:
        shard.close_window()
        pump.cancel()
        await asyncio.gather(pump, return_exceptions=True)
        await shard.close()
    try:
        shard.write()
    except OSError as exc:
        emit("error", shard=config.shard, message=f"cannot write samples: {exc!r}")
        return 1
    emit(
        "done",
        shard=config.shard,
        turns=len(shard.turns),
        text_frames=shard.text_frames,
        errors=shard.errors,
    )
    return 0


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="driver_shard.py",
        description="capacity harness: one driver shard (spawned by run.py)",
    )
    parser.add_argument("--config", type=Path, required=True, help="shard config JSON")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        return asyncio.run(run(args.config))
    except Exception as exc:  # noqa: BLE001 - 任何未捕获失败都要以握手行上报
        emit("error", shard=-1, message=f"{type(exc).__name__}: {exc}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
