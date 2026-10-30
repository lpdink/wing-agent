#!/usr/bin/env python3
"""capacity 的入口微基准：把「一帧事件在网关里要花多少 CPU」拆成可复现的 case。

`docs/dev/capacity.md` §6 的成本归因引用了这些读数，本文件是它们的复现路径。
它**只**测事件通路（上游 SSE 帧 → 解析 → 事件对象 → 广播 → WS 出网）的每事件 CPU；
每轮固定成本（上下文装配 / 落盘 fsync / 审计 INSERT / 整段序列化）不在本文件里，
那是另一条测量轴，随**轮次频率**变化。

case：

    read      单连接 20 ms 节奏下「每帧一次唤醒」的固定成本 vs 每事件边际成本
              （扫「一次读吃几帧」），再加目标档形态的多连接读数（1 → N 条连接 × 50 帧/s）。
    eventbus  EventBus 的两处 O(N) 扇出簿记：`session → clients` 反查、活跃会话的订阅者解析。
    emit      每事件的对象构造 + `wire_dump` + `json.dumps` + `build_frames`。
    egress    每事件的 `create_task` + `wait_for(ws.send_text)`（真 uvicorn + starlette WS）。
    all       上面四个 + 目标档预算表（µs/事件 → 折合多少核）。

口径（与 `docs/dev/capacity.md` §6 一致）：

- 一律 `time.process_time()`（进程 CPU，不含阻塞等待）。上游（假 Provider）与出口的接收端
  都跑在**子进程**里，它们的 CPU 不进读数——与真实部署里「模型服务在另一个进程/另一台机器」同形。
- 折算：`核 = µs/事件 × 事件率 / 1e6`；目标档事件率 = 1000 连接 × 50 帧/s = 50 000 事件/s。
  这是「满占空比」的定义（每条连接每秒 50 帧），不代表真实模型的 token 速率。
- **两类读数不可外推**（§6 的结论正建立在这两条上）：
  (a) 单连接、低速率的读数是「空闲进程被唤醒一次」的价格（缓存冷）。同一份代码在 1000 连接时
      每事件只要 1/10；把 N=1 的读数乘上 50k 事件/s 会高估一个数量级。
  (b) 单连接、一次读吃多帧的读数把「多帧摊一次读」的红利算进去了；50 帧/s/连接时每次读
      只带回一帧（`reads/event ≈ 1`），这份红利在多连接形态下拿不到。
- 所以「每事件成本随速率下降」的成因是**进程热起来后每次唤醒的固定开销被摊薄**，
  不是批量解析本身。`read` case 会同时打印 `reads/event`，让这一点自证。

退出码：0 = 跑完；2 = 环境不满足（fd 上限不足 / 上游没起来）。
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import platform
import resource
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import AsyncIterator, Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import httpx

from wing.event import EventTarget, TextEvent, wire_dump
from wing.event_bus import EventBus
from wing.gateway.frames import build_frames
from wing.provider.transport import (
    lines_with_idle_timeout,
    parse_sse_stream,
)

# ── 目标档与常量 ──────────────────────────────────────────────

#: 目标档：1000 条连接，每条每秒 50 帧（= 每个上游 delta 一帧）。
TARGET_CONNS = 1000
TARGET_FPS = 50
#: 目标档事件率。预算表里所有的「核」都按它折算。
TARGET_EVENTS_PER_S = TARGET_CONNS * TARGET_FPS
#: 单连接节奏：20 ms/帧 = 50 帧/s，与目标档的每连接帧率一致。
PACE_S = 0.02
#: 抬到多高（默认软限 256 开不下 1000 条连接）。
FD_TARGET = 8192
#: 目标档每个事件的 WS 载荷大小：一个文本 delta 的典型大小。
PAYLOAD_HINT_BYTES = 162

#: 上游写出的那一帧（每帧 2 行：`data:` + 空行）。子进程源码由它派生。
SSE_FRAME = b'data: {"choices":[{"delta":{"content":"hello world"}}]}\n\n'


def cores(us_per_event: float) -> float:
    """每事件成本（µs）→ 目标档折合核数。"""
    return us_per_event * TARGET_EVENTS_PER_S / 1e6


def _median(samples: Sequence[float]) -> float:
    return statistics.median(samples)


def _pick_stat(samples: Sequence[float], stat: str) -> float:
    if stat == "min":
        return min(samples)
    return _median(samples)


def _spread(samples: Sequence[float]) -> float:
    """相对极差 (max-min)/median：判断读数够不够两位有效数字。"""
    med = _median(samples)
    return (max(samples) - min(samples)) / med if med else 0.0


@dataclass(slots=True)
class Reading:
    """一条读数：中位数 + 全部样本 + 上下文，`key` 供预算表稳定取用。"""

    key: str
    label: str
    us_per_event: float
    samples: list[float] = field(default_factory=list)
    extra: dict[str, float] = field(default_factory=dict)
    #: 取值口径：含 I/O 的读数取 ``min``（干扰只会抬高读数），纯 CPU 的取 ``median``。
    stat: str = "median"

    def as_dict(self) -> dict[str, Any]:
        return {
            "key": self.key,
            "label": self.label,
            "us_per_event": round(self.us_per_event, 3),
            "cores_at_target": round(cores(self.us_per_event), 3),
            "stat": self.stat,
            "samples": [round(s, 2) for s in self.samples],
            "spread": round(_spread(self.samples), 4),
            "extra": {k: round(v, 4) for k, v in self.extra.items()},
        }


@dataclass(slots=True)
class CaseResult:
    name: str
    readings: list[Reading] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)

    def by_key(self, key: str) -> Reading:
        for reading in self.readings:
            if reading.key == key:
                return reading
        raise KeyError(f"{self.name}: no reading {key!r}")


# ── 上游：真 TCP + 真 SSE 字节流（子进程） ──────────────────────

# 服务端源码在运行期写到临时文件再以子进程启动：本文件因此只有一份源码，
# 且服务端的 CPU 不会混进被测进程的 process_time。
_SERVER_TEMPLATE = '''\
"""microbench 上游：每条连接按固定节奏写 SSE 帧（由 microbench 生成，非仓库模块）。"""

from __future__ import annotations

import argparse
import asyncio
import socket


FRAME = @@FRAME@@
HEAD = (
    b"HTTP/1.1 200 OK\\r\\n"
    b"Content-Type: text/event-stream\\r\\n"
    b"Connection: close\\r\\n\\r\\n"
)


async def serve(writer, frames, group, pace):
    sock = writer.get_extra_info("socket")
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    try:
        writer.write(HEAD)
        await writer.drain()
        written = 0
        while written < frames:
            k = min(group, frames - written)
            writer.write(FRAME * k)
            written += k
            await writer.drain()
            if pace:
                await asyncio.sleep(pace)
    except (ConnectionResetError, BrokenPipeError):
        pass
    finally:
        writer.close()


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--frames", type=int, default=100)
    ap.add_argument("--group", type=int, default=1)
    ap.add_argument("--pace", type=float, default=0.0)
    ap.add_argument("--conns", type=int, default=1)
    args = ap.parse_args()

    barrier = asyncio.Event()
    arrived = {"n": 0}

    async def handle(reader, writer):
        try:
            await reader.readuntil(b"\\r\\n\\r\\n")
        except Exception:
            pass
        if args.conns > 1:
            arrived["n"] += 1
            if arrived["n"] >= args.conns:
                barrier.set()
            await barrier.wait()
        await serve(writer, args.frames, args.group, args.pace)

    server = await asyncio.start_server(handle, "127.0.0.1", 0, backlog=2048)
    port = server.sockets[0].getsockname()[1]
    print(f"READY {port}", flush=True)
    async with server:
        await asyncio.Event().wait()


if __name__ == "__main__":
    asyncio.run(main())
'''

SERVER_SRC = _SERVER_TEMPLATE.replace("@@FRAME@@", repr(SSE_FRAME))


class SseFarm:
    """一个上游 SSE 服务（子进程）+ 端口发现；一个 case 内复用它跑多次重复。

    `conns > 1` 时上游先等齐 `conns` 条连接再开始写（屏障），于是「全部响应头到手」
    与「上游开始吐帧」是同一时刻，稳态窗口里不会有连接提前跑掉。
    """

    def __init__(self, *, frames: int, group: int, pace: float, conns: int = 1) -> None:
        self._frames = frames
        self._group = group
        self._pace = pace
        self._conns = conns
        self._proc: subprocess.Popen[bytes] | None = None
        self._script: str | None = None
        self.port = 0

    def __enter__(self) -> SseFarm:
        fd, path = tempfile.mkstemp(prefix="capacity-sse-", suffix=".py")
        with os.fdopen(fd, "w") as handle:
            handle.write(SERVER_SRC)
        self._script = path
        self._proc = subprocess.Popen(
            [
                sys.executable,
                path,
                "--frames",
                str(self._frames),
                "--group",
                str(self._group),
                "--pace",
                str(self._pace),
                "--conns",
                str(self._conns),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        assert self._proc.stdout is not None
        ready = self._proc.stdout.readline().decode().strip()
        if not ready.startswith("READY "):
            self.__exit__()
            raise RuntimeError(f"upstream did not start: {ready!r}")
        self.port = int(ready.split()[1])
        return self

    def __exit__(self, *exc: object) -> None:
        if self._proc is not None:
            self._proc.kill()
            self._proc.wait()
            self._proc = None
        if self._script is not None:
            Path(self._script).unlink(missing_ok=True)
            self._script = None

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/v1/chat/completions"


# ── read：读取路径的每帧成本 ──────────────────────────────────


#: 现状链（与 `provider/openai/provider.py` 的泵同形）：
#: `aiter_lines()` → `lines_with_idle_timeout`（每行一次 `wait_for`）→ `parse_sse_stream`。
#: 读次数靠临时替换实例方法 `aiter_raw` 统计（httpx 的三层解码都经它取字节）；
#: 只多一层空 async generator（0.06 µs/读），对读数的影响 < 0.5%。
async def _consume_chain(resp: httpx.Response, reads: dict[str, int]) -> int:
    original = resp.aiter_raw

    def counted() -> AsyncIterator[bytes]:
        async def gen() -> AsyncIterator[bytes]:
            async for chunk in original():
                reads["n"] += 1
                yield chunk

        return gen()

    resp.aiter_raw = counted  # type: ignore[method-assign]
    events = 0
    async for _event in parse_sse_stream(
        lines_with_idle_timeout(resp.aiter_lines(), context="capacity-microbench")
    ):
        events += 1
    return events


async def _stream_once(
    client: httpx.AsyncClient, url: str, expect: int
) -> tuple[float, int, int]:
    """跑一条连接，返回 (消费期 CPU 秒, 事件数, 读次数)。"""
    reads = {"n": 0}
    resp = await client.send(client.build_request("GET", url), stream=True)
    started = time.process_time()
    events = await _consume_chain(resp, reads)
    elapsed = time.process_time() - started
    await resp.aclose()
    if events != expect:
        raise RuntimeError(f"expected {expect} events, parsed {events}")
    return elapsed, events, reads["n"]


async def _read_wakeup(
    groups: Sequence[int], writes: int, repeats: int
) -> tuple[list[Reading], list[str]]:
    """单连接、20 ms/帧：扫「一次读吃几帧」，分离每次唤醒的固定成本与每事件边际成本。

    测的是：一次 socket 唤醒的**固定**开销（读系统调用 + httpx 四层解码 + 每行
    `wait_for` + SSE 解析），以及每次读带回的每帧要额外付的边际成本。
    折算：`µs/读 = CPU / 实测读次数`（一次 write 不保证等于一次 read），
    再对「每读帧数 → µs/读」做最小二乘，分离出固定项与每帧边际项。
    每档固定写 `writes` 次（而不是固定帧数）：于是每个样本都平均同样多次唤醒，
    高「每读帧数」档不会退化成「只采到三五次唤醒」。
    不可外推：这是**空闲进程**（两次唤醒之间睡 20 ms）的唤醒价。进程变忙后同一份
    代码每次唤醒便宜一半以上 —— 见 `_read_scale` 的 1000 连接读数。
    """
    out: list[Reading] = []
    out_notes: list[str] = []
    async with httpx.AsyncClient(timeout=httpx.Timeout(120.0, connect=5.0)) as client:
        with SseFarm(frames=20, group=1, pace=PACE_S) as farm:
            await _stream_once(client, farm.url, 20)  # 丢弃：首条连接的初始化
        for group in groups:
            frames = writes * group
            samples: list[float] = []
            per_read: list[float] = []
            dropped = 0
            with SseFarm(frames=frames, group=group, pace=PACE_S) as farm:
                for _ in range(repeats):
                    elapsed, events, reads = await _stream_once(
                        client, farm.url, frames
                    )
                    if reads < 0.8 * writes:
                        # 这一样本里上游的多次写被一次读吃掉了 —— 那不是目标负载
                        # （每连接 50 帧/s ⇒ 一帧一次读），扔掉而不计入中位数。
                        dropped += 1
                        continue
                    samples.append(elapsed / events * 1e6)
                    # 一次 write 不一定等于一次 read（大块会被切成多段到达），
                    # 所以 µs/读 用实测读次数，而不是 assumed 的每读帧数。
                    per_read.append(elapsed / max(reads, 1) * 1e6)
            if not samples:
                raise RuntimeError(
                    f"group={group}: 全部样本都发生了批量读，本机负载太高，重跑"
                )
            if dropped:
                out_notes.append(
                    f"group={group}: 丢弃 {dropped} 个「一次读吃多帧」的样本"
                )
            out.append(
                Reading(
                    key=f"wakeup:{group}",
                    label=f"每次读吃 {group:2d} 帧",
                    us_per_event=_pick_stat(samples, "min"),
                    samples=samples,
                    stat="min",
                    extra={
                        "frames_per_write": float(group),
                        "us_per_read": _median(per_read),
                        "reads_per_write": reads / frames * group,
                    },
                )
            )
    return out, out_notes


async def _run_conns(url: str, conns: int, frames: int) -> tuple[float, float]:
    """并发跑 `conns` 条连接；返回稳态窗口里的 (µs/事件, reads/event)。"""
    limits = httpx.Limits(max_connections=conns + 8)
    ready = asyncio.Event()
    arrived = {"n": 0}
    # 收集而不是跨 await 累加：`totals[k] += await f()` 会在 await 前先读旧值，
    # 并发任务互相覆盖（这正是本 case 最早读数只有一条连接的原因）。
    tally: list[tuple[int, int]] = []
    async with httpx.AsyncClient(
        timeout=httpx.Timeout(120.0, connect=5.0), limits=limits
    ) as client:

        async def one(index: int) -> None:
            # 错开建连：一次灌 N 个 SYN 会撞上游的 listen backlog（随机 ConnectError）。
            if index:
                await asyncio.sleep(index * 0.001)
            resp = await client.send(client.build_request("GET", url), stream=True)
            arrived["n"] += 1
            if arrived["n"] >= conns:
                ready.set()
            reads = {"n": 0}
            events = await _consume_chain(resp, reads)
            tally.append((events, reads["n"]))
            await resp.aclose()

        gather = asyncio.gather(*(one(i) for i in range(conns)))
        await ready.wait()
        started = time.process_time()
        await gather
        elapsed = time.process_time() - started

    events = sum(item[0] for item in tally)
    if events != conns * frames:
        raise RuntimeError(
            f"expected {conns * frames} events, got {events}"
            "（fd 上限不足时先抬 ulimit，或把档位调小）"
        )
    reads_per_event = sum(item[1] for item in tally) / events
    return elapsed / events * 1e6, reads_per_event


async def _read_scale(
    levels: Sequence[int], frames: int, repeats: int
) -> tuple[list[Reading], list[str]]:
    """目标档形态：N 条连接各 50 帧/s，每条一帧一次读。

    测的是：生产里真正要付的读取路径成本（httpx 栈 + SSE 解析 + 每行 `wait_for`）。
    折算：`核 = µs/事件 × 50 000 / 1e6`。这一档是预算表里「读 + SSE 解析」的取值来源。
    不可外推：`reads/event ≈ 1` 说明每次读只带回一帧 —— 多帧摊一次读的优化在这里无处摊；
    反过来这一档也不能往下推：它已经是「进程忙」的状态，低负载时同一份代码还要更贵。
    """
    out: list[Reading] = []
    notes: list[str] = []
    for conns in levels:
        samples: list[float] = []
        reads_per_event = 0.0
        dropped = 0
        for _ in range(repeats):
            # 每个重复起一份新上游：屏障只放行一次，复用会让第二次没有同步点。
            with SseFarm(frames=frames, group=1, pace=PACE_S, conns=conns) as farm:
                us_per_event, ratio = await _run_conns(farm.url, conns, frames)
            if ratio < 0.8:
                # 目标负载是「每条连接 50 帧/s ⇒ 一帧一次读」。读数远小于 1 说明进程
                # 跟不上、事件在 socket 里堆积被成批读走 —— 这一样本不代表目标形态。
                dropped += 1
                continue
            samples.append(us_per_event)
            reads_per_event = ratio
        if not samples:
            samples.append(us_per_event)
            notes.append(
                f"⚠ {conns} 连接档的所有样本都发生了批量读（reads/event < 0.8）："
                "进程跟不上 50 帧/s/连接，读数偏低，换台安静点的机器重跑"
            )
            reads_per_event = ratio
        elif dropped:
            notes.append(f"{conns} 连接档丢弃 {dropped} 个批量读样本")
        out.append(
            Reading(
                key=f"scale:{conns}",
                label=f"{conns} 条连接 × {TARGET_FPS} 帧/s",
                us_per_event=_pick_stat(samples, "min"),
                samples=samples,
                stat="min",
                extra={
                    "conns": float(conns),
                    "reads_per_event": reads_per_event,
                    "events_per_s": float(conns * TARGET_FPS),
                },
            )
        )
    return out, notes


# ── eventbus：两处 O(N) 扇出簿记 ────────────────────────────────


def _emit_samples(bus: EventBus, event: TextEvent, iterations: int) -> list[float]:
    """把一个 session 定向事件 emit `iterations` 次，返回 3 次重复的 µs/次。

    复用同一个 `EventTarget` 对象：否则每次都摊一次 pydantic 构造（约 0.3 µs），
    会把「扫描成本」淹没。`emit` 自己会替换 `event.target`，所以每次重新挂上。
    """
    scope = EventTarget(scope="session")
    samples: list[float] = []
    for _ in range(3):
        started = time.process_time()
        for _ in range(iterations):
            event.target = scope
            bus.emit(event)
        samples.append((time.process_time() - started) / iterations * 1e6)
    return samples


def _routing_scan(size: int, iterations: int) -> Reading:
    """`emit` 解析 `scope="session"` 的收件人时，遍历整张 `client_id → {session}` 表。

    测的是：网关里有 `size` 条已订阅连接时，**每个会话事件**为找收件人付的扫描成本。
    折算：纯 CPU，`核 = µs/事件 × 50 000 / 1e6`；它随连接数线性增长（O(N)）。
    不可外推：扫描项数 = 已订阅的客户端数。一个客户端订阅多个会话时项数不变、命中变多，
    读数会略低于线性（本 case 是每客户端恰好订阅 1 个会话的形态）。
    """
    bus = EventBus()
    for i in range(size):
        bus.route_attach(f"client-{i}", f"session-{i}")
    event = TextEvent(session_id="session-0", content="hello world")
    samples = _emit_samples(bus, event, iterations)
    return Reading(
        key=f"routing:{size}",
        label=f"路由扫描 @ {size} 条连接",
        us_per_event=_median(samples),
        samples=samples,
        extra={"clients": float(size)},
    )


def _subscriber_scan(size: int, iterations: int) -> Reading:
    """每个活跃会话往**全局扁平**订阅者列表注册一个回调，`emit` 对每个事件遍历整表。

    回调首行是 `event.session_id != self._session_id` 早退（`session/session.py` 的用量账本），
    所以 `size` 次调用里只有 1 次真干活，其余全是空转。
    折算：纯 CPU，随活跃会话数线性增长（O(N)）。
    不可外推：项数 = 已接纳进工作集的会话数（被逐出的会话会退订）。容量驱动里连接与会话
    一比一，真实部署可以更少。
    """
    bus = EventBus()
    bus.route_attach("client-0", "session-0")

    def subscribe(session_id: str) -> None:
        def on_event(candidate: TextEvent) -> None:
            if candidate.session_id != session_id:
                return

        bus.subscribe(on_event)  # type: ignore[arg-type]

    for i in range(size):
        subscribe(f"session-{i}")
    event = TextEvent(session_id="session-0", content="hello world")
    samples = _emit_samples(bus, event, iterations)
    return Reading(
        key=f"subscribers:{size}",
        label=f"订阅者解析 @ {size} 个活跃会话",
        us_per_event=_median(samples),
        samples=samples,
        extra={"sessions": float(size)},
    )


def _case_eventbus(sizes: Sequence[int], iterations: int) -> CaseResult:
    result = CaseResult("eventbus")
    for size in sizes:
        result.readings.append(_routing_scan(size, iterations))
        result.readings.append(_subscriber_scan(size, iterations))
    baseline = _routing_scan(1, iterations)
    baseline.key = "routing:1"
    baseline.label = "基线（1 条连接 / 1 个会话）"
    result.readings.append(baseline)
    result.notes.append(
        "两处都是 O(项数)：真正的 emit 固定成本约 1 µs（见「基线」行），其余全是扫描"
    )
    return result


# ── emit：每个事件的对象与序列化 ────────────────────────────────


def _stage(fn: Callable[[], None], n: int) -> list[float]:
    samples: list[float] = []
    for _ in range(3):
        started = time.process_time()
        fn()
        samples.append((time.process_time() - started) / n * 1e6)
    return samples


def _case_emit(n: int) -> CaseResult:
    """`TextEvent(...)` + `wire_dump(...)` + `json.dumps(...)` + `build_frames(...)`。

    测的是：网关把「一个事件」变成「一帧 WS 载荷」的纯 CPU（不含 socket）。
    折算：纯 CPU，`核 = µs/事件 × 50 000 / 1e6`。
    不可外推：`persist=True` 的事件另有落盘成本（不在这里）；载荷更大（工具结果、媒体引用）
    时 `json.dumps` 随字节线性变贵 —— 本 case 用 ~162 B 的文本 delta。
    """
    session_id = "9f3a1c8e2b7d4a6190abcdef01234567"
    event = TextEvent(
        session_id=session_id, content="hello world", request_id="req_8f2a"
    )
    payload = json.dumps(wire_dump(event))

    def composite() -> None:
        for _ in range(n):
            fresh = TextEvent(
                session_id=session_id, content="hello world", request_id="req_8f2a"
            )
            build_frames(json.dumps(wire_dump(fresh)), "text")

    def dumps_only() -> None:
        for _ in range(n):
            json.dumps(wire_dump(event))

    def frames_only() -> None:
        for _ in range(n):
            build_frames(payload, "text")

    result = CaseResult("emit")
    result.readings.append(
        Reading(
            key="composite",
            label="TextEvent + wire_dump + json.dumps + build_frames",
            us_per_event=_median(_stage(composite, n)),
            samples=_stage(composite, n),
            extra={"payload_bytes": float(len(payload))},
        )
    )
    result.readings.append(
        Reading(
            key="dumps",
            label="  ├ wire_dump + json.dumps",
            us_per_event=_median(_stage(dumps_only, n)),
            samples=_stage(dumps_only, n),
        )
    )
    result.readings.append(
        Reading(
            key="frames",
            label="  └ build_frames",
            us_per_event=_median(_stage(frames_only, n)),
            samples=_stage(frames_only, n),
        )
    )
    return result


# ── egress：每事件一次投递 ──────────────────────────────────────

# 接收端（子进程）：只收，什么都不做 —— 于是 `create_task` / `wait_for(send_text)` 的读数
# 里不含对端的工作。用 `websockets` 是因为 `uvicorn[standard]` 已经依赖它。
_RECEIVER_SRC = '''\
"""microbench 出口的接收端（由 microbench 生成，非仓库模块）。"""

from __future__ import annotations

import argparse

from websockets.sync.client import connect


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--frames", type=int, required=True)
    args = ap.parse_args()
    with connect(f"ws://127.0.0.1:{args.port}/ws", max_size=None) as ws:
        for _ in range(args.frames):
            ws.recv()
    print("DONE", flush=True)


if __name__ == "__main__":
    main()
'''

#: 与 `GatewayServer._send_text` 同形的有界等待。
WRITE_TIMEOUT_S = 60.0


async def _egress_once(events: int, coalesce: int) -> float:
    """起一个真 uvicorn + starlette WS，把 `events` 个事件推给子进程里的接收端。

    `coalesce=1` 是现状：每事件一次 `create_task` + 一次 `wait_for(send_text)`。
    `coalesce=M` 把 M 个事件合成一帧 —— 这条杠杆的上限就在这两档之差里。
    """
    import uvicorn
    from starlette.applications import Starlette
    from starlette.routing import WebSocketRoute
    from starlette.websockets import WebSocket

    session_id = "9f3a1c8e2b7d4a6190abcdef01234567"
    state: dict[str, float] = {}

    async def send_frames(ws: WebSocket, frames: list[Any]) -> None:
        for frame in frames:
            await asyncio.wait_for(ws.send_text(frame.text), timeout=WRITE_TIMEOUT_S)

    async def endpoint(ws: WebSocket) -> None:
        await ws.accept()
        payloads = [
            json.dumps(
                wire_dump(
                    TextEvent(
                        session_id=session_id,
                        content="hello world",
                        request_id="req_8f2a",
                    )
                )
            )
            for _ in range(events)
        ]
        loop = asyncio.get_running_loop()
        started = time.process_time()
        tasks = [
            loop.create_task(
                send_frames(
                    ws,
                    build_frames(
                        payloads[i]
                        if coalesce == 1
                        else "[" + ",".join(payloads[i : i + coalesce]) + "]",
                        "text",
                    ),
                )
            )
            for i in range(0, events, coalesce)
        ]
        await asyncio.gather(*tasks)
        state["cpu"] = time.process_time() - started

    fd, script = tempfile.mkstemp(prefix="capacity-ws-", suffix=".py")
    with os.fdopen(fd, "w") as handle:
        handle.write(_RECEIVER_SRC)
    app = Starlette(routes=[WebSocketRoute("/ws", endpoint)])
    server = uvicorn.Server(
        uvicorn.Config(app, host="127.0.0.1", port=0, log_level="error", lifespan="off")
    )
    serving = asyncio.get_running_loop().create_task(server.serve())
    while not server.started:
        await asyncio.sleep(0.01)
    port = server.servers[0].sockets[0].getsockname()[1]
    receiver = subprocess.Popen(
        [
            sys.executable,
            script,
            "--port",
            str(port),
            "--frames",
            str(events // coalesce),
        ],
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        assert receiver.stdout is not None
        line = await asyncio.get_running_loop().run_in_executor(
            None, receiver.stdout.readline
        )
        if line.strip() != "DONE":
            raise RuntimeError(f"receiver failed: {line!r}")
    finally:
        receiver.wait()
        server.should_exit = True
        await serving
        Path(script).unlink(missing_ok=True)
    return state["cpu"] / events * 1e6


async def _egress_async(events: int, coalesce: int, repeats: int) -> list[float]:
    return [await _egress_once(events, coalesce) for _ in range(repeats)]


def _egress_warmup(events: int, coalesce: int) -> None:
    """丢弃一次：首条 WS 连接要付 uvicorn 的协议升级与 `websockets` 首帧的初始化。"""
    run_sync(_egress_async(events, coalesce, 1))


def _case_egress(events: int, repeats: int, coalesced: int) -> CaseResult:
    """每事件一次 `create_task` + `wait_for(ws.send_text)`（真 uvicorn + starlette WS）。

    测的是：网关把一个已序列化的帧交给 ASGI/uvicorn 并写进 socket 的固定开销
    （任务创建、定时器挂/摘、WS 帧构造、写系统调用），不含对端。
    折算：`核 = µs/事件 × 50 000 / 1e6`；合成 M 帧/次后摊到每事件的就是这条杠杆的上限。
    不可外推：接收端在本机 loopback 且只收不发；跨网络的写会阻塞（走 `write_timeout` 分支）。
    """
    result = CaseResult("egress")
    for coalesce in (1, coalesced):
        _egress_warmup(min(events, 500), coalesce)
        samples = run_sync(_egress_async(events, coalesce, repeats))
        result.readings.append(
            Reading(
                key=f"coalesce:{coalesce}",
                label=f"每 {coalesce:2d} 个事件一帧",
                us_per_event=_pick_stat(samples, "min"),
                samples=samples,
                stat="min",
                extra={"coalesce": float(coalesce)},
            )
        )
    return result


def _fit_fixed_marginal(readings: Sequence[Reading]) -> tuple[float, float]:
    """对「每读帧数 → µs/读」做最小二乘，返回 (每次唤醒的固定成本, 每帧的边际成本)。

    这两个数就是 §6 归因的形状：唤醒的固定成本在**空闲进程**上极高（缓存冷），
    而每帧的边际成本与进程忙闲无关；目标档每连接 50 帧/s ⇒ 唤醒次数 = 帧数，
    因此付的是「固定 + 边际」，而 1000 连接时同一份代码把固定项摊到几乎为零。
    """
    points = [(r.extra["frames_per_write"], r.extra["us_per_read"]) for r in readings]
    if len(points) < 2:
        return (0.0, 0.0)
    n = float(len(points))
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    denom = sum((x - mean_x) ** 2 for x, _ in points)
    slope = (
        sum((x - mean_x) * (y - mean_y) for x, y in points) / denom if denom else 0.0
    )
    return (mean_y - slope * mean_x, slope)


# ── 预算表 ──────────────────────────────────────────────────────


def _budget(cases: dict[str, CaseResult], conns: int) -> dict[str, Any]:
    """把四个 case 拼成目标档预算（µs/事件 → 核）。

    分量取值：读取 = `read` 里 `conns` 那一档（目标档形态）；扇出 = `eventbus` 里
    `conns` 条连接 + `conns` 个活跃会话那两条之和；对象/序列化 = `emit` 的复合读数；
    出网 = `egress` 的 `coalesce=1`。「扇出索引化后」把扇出降到 O(1) 的固定成本（约 1 µs）。
    """
    read = cases["read"].by_key(f"scale:{conns}")
    routing = cases["eventbus"].by_key(f"routing:{conns}")
    subscribers = cases["eventbus"].by_key(f"subscribers:{conns}")
    emit = cases["emit"].by_key("composite")
    egress = cases["egress"].by_key("coalesce:1")
    fanout = routing.us_per_event + subscribers.us_per_event
    components = [
        ("读 + SSE 解析", read.us_per_event),
        ("EventBus 扇出（路由 + 订阅者）", fanout),
        ("事件对象 + 序列化", emit.us_per_event),
        ("WS 出网", egress.us_per_event),
    ]
    total = sum(value for _, value in components)
    indexed = total - fanout + 1.0
    return {
        "target": {
            "conns": conns,
            "fps": TARGET_FPS,
            "events_per_s": conns * TARGET_FPS,
        },
        "components": [
            {
                "name": name,
                "us_per_event": round(value, 2),
                "cores": round(cores(value), 3),
            }
            for name, value in components
        ],
        "total": {"us_per_event": round(total, 2), "cores": round(cores(total), 3)},
        "after_eventbus_index": {
            "us_per_event": round(indexed, 2),
            "cores": round(cores(indexed), 3),
            "speedup": round(total / indexed, 3),
        },
        "reads_per_event": round(read.extra.get("reads_per_event", 0.0), 4),
    }


# ── 执行与打印 ──────────────────────────────────────────────────


def run_sync(coro: Any) -> Any:
    """用 uvloop 跑（`uvicorn` 的 `loop="auto"` 就是它，即生产口径）。"""
    try:
        import uvloop
    except ImportError:
        return asyncio.run(coro)
    return uvloop.run(coro)


def has_uvloop() -> bool:
    try:
        import uvloop  # noqa: F401
    except ImportError:
        return False
    return True


def limit_fds() -> int:
    """抬软上限到硬上限（默认 256 开不下 1000 条连接）。返回生效的软上限。"""
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    target = FD_TARGET if hard == resource.RLIM_INFINITY else min(FD_TARGET, hard)
    if target > soft:
        try:
            resource.setrlimit(resource.RLIMIT_NOFILE, (target, hard))
            soft = target
        except (ValueError, OSError):
            pass
    return int(soft)


def _print_case(result: CaseResult) -> None:
    print(f"\n■ {result.name}")
    for reading in result.readings:
        extras = "  ".join(f"{k}={v:g}" for k, v in reading.extra.items())
        print(
            f"  {reading.label}：{reading.us_per_event:.2f} µs/事件"
            f" → {cores(reading.us_per_event):.2f} 核"
            f"（{'最小' if reading.stat == 'min' else '中位'}"
            f" 样本 {[round(s, 2) for s in reading.samples]}"
            f" 极差 {_spread(reading.samples):.0%}）"
            + (f"  [{extras}]" if extras else "")
        )
    for note in result.notes:
        print(f"  · {note}")


def _print_budget(budget: dict[str, Any]) -> None:
    target = budget["target"]
    print(
        f"\n■ 目标档预算：{target['conns']} 连接 × {target['fps']} 帧/s"
        f" = {target['events_per_s']} 事件/s"
        f"（reads/event = {budget['reads_per_event']:.3f}）"
    )
    for row in budget["components"]:
        print(
            f"  {row['name']}：{row['us_per_event']:.2f} µs/事件 → {row['cores']:.2f} 核"
        )
    total = budget["total"]
    print(f"  合计：{total['us_per_event']:.2f} µs/事件 → {total['cores']:.2f} 核")
    indexed = budget["after_eventbus_index"]
    print(
        f"  扇出索引化后（两处 O(N) → O(1)）：{indexed['us_per_event']:.2f} µs/事件"
        f" → {indexed['cores']:.2f} 核（{indexed['speedup']:.2f}x）"
    )
    print(
        "  注：不含每轮固定成本（上下文装配 / 落盘 fsync / 审计 INSERT / 整段序列化）——"
        "那条轴随轮次频率变。"
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="capacity 的入口微基准（docs/dev/capacity.md §6 的数字复现路径）",
    )
    parser.add_argument(
        "--case",
        default="all",
        choices=("read", "eventbus", "emit", "egress", "all"),
        help="跑哪个 case（默认 all）",
    )
    pacing = parser.add_mutually_exclusive_group()
    pacing.add_argument(
        "--quick",
        action="store_true",
        help="短档位（默认）：30 秒内跑完，够两位有效数字",
    )
    pacing.add_argument(
        "--full",
        action="store_true",
        help="更长的驻留与更多档位（数字更稳，耗时数倍）",
    )
    parser.add_argument("--json", action="store_true", help="只打印机器可读结果")
    parser.add_argument("--repeats", type=int, default=None, help="覆盖重复次数")
    parser.add_argument(
        "--conns", type=int, default=None, help="覆盖目标档连接数（默认 1000）"
    )
    args = parser.parse_args(list(argv) if argv is not None else None)

    repeats = args.repeats or (4 if args.full else 3)
    conns = args.conns or TARGET_CONNS
    full = args.full
    fd_limit = limit_fds()
    if fd_limit < conns + 64:
        print(
            f"fd 上限 {fd_limit} 开不下 {conns} 条连接；请调低 --conns 或抬高 ulimit",
            file=sys.stderr,
        )
        return 2

    started = time.monotonic()
    if not args.json:
        print(
            f"capacity microbench — python {platform.python_version()}, "
            f"loop={'uvloop' if has_uvloop() else 'asyncio'}, "
            f"mode={'full' if full else 'quick'}, fd={fd_limit}, "
            f"目标档 {conns}×{TARGET_FPS} 帧/s"
        )

    cases: dict[str, CaseResult] = {}

    wanted = (
        ("read", "eventbus", "emit", "egress") if args.case == "all" else (args.case,)
    )

    if "read" in wanted:
        result = CaseResult("read")
        wakeup, wakeup_notes = run_sync(
            _read_wakeup(
                groups=(1, 4, 16) if not full else (1, 2, 4, 8, 16),
                writes=30 if not full else 80,
                repeats=repeats,
            )
        )
        result.readings += wakeup
        result.notes += wakeup_notes
        # quick 只跑目标档：单连接的「冷唤醒价」由 read 的第一档给（同一形态、口径一致），
        # 再跑一遍 1 连接会与它互相矛盾（进程已经被前面的档焐热了）。
        levels = (conns,) if not full else (1, 10, 100, conns)
        scale, scale_notes = run_sync(
            _read_scale(levels, frames=30 if not full else 100, repeats=repeats)
        )
        result.readings += scale
        result.notes += scale_notes
        fixed, marginal = _fit_fixed_marginal(wakeup)
        hot = result.by_key(f"scale:{conns}").us_per_event
        result.notes.append(
            f"最小二乘：每次唤醒固定 {fixed:.0f} µs + 每帧 {marginal:.1f} µs；"
            f"{conns} 连接档每事件 {hot:.1f} µs ≈ 边际项"
        )
        result.notes.append(
            "⇒ 每事件成本随规模下降，是因为**固定项被摊薄**（进程变忙、唤醒变便宜），"
            "不是批量解析：多连接档 reads/event ≈1，没有第二帧可以并进同一次读"
        )
        result.notes.append(f"上游每帧 {len(SSE_FRAME)} B，{PACE_S * 1000:.0f} ms/帧")
        cases["read"] = result

    if "eventbus" in wanted:
        sizes = (conns, conns * 5) if not full else (conns, conns * 2, conns * 5)
        cases["eventbus"] = _case_eventbus(sizes, 8000 if not full else 20000)

    if "emit" in wanted:
        cases["emit"] = _case_emit(50000 if not full else 200000)

    if "egress" in wanted:
        cases["egress"] = _case_egress(8000 if not full else 20000, repeats, 16)

    budget = _budget(cases, conns) if args.case == "all" else None

    if args.json:
        print(
            json.dumps(
                {
                    "meta": {
                        "python": platform.python_version(),
                        "platform": platform.platform(),
                        "loop": "uvloop" if has_uvloop() else "asyncio",
                        "mode": "full" if full else "quick",
                        "fd_limit": fd_limit,
                        "repeats": repeats,
                        "payload_hint_bytes": PAYLOAD_HINT_BYTES,
                    },
                    "cases": {
                        name: {
                            "readings": [r.as_dict() for r in res.readings],
                            "notes": res.notes,
                        }
                        for name, res in cases.items()
                    },
                    "budget": budget,
                },
                ensure_ascii=False,
                indent=2,
            )
        )
        return 0

    for result in cases.values():
        _print_case(result)
    if budget is not None:
        _print_budget(budget)
    print(f"\n总耗时 {time.monotonic() - started:.1f} s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
