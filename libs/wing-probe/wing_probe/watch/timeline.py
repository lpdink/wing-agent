"""事件时间线 —— WS 入站事件的有序缓冲 + 未消费游标（design D4）。

时间线是断言的数据源，只做三件事：

1. **原样收**：每个完整事件记 ``type`` / ``data`` / 相对单调时钟 / 原始载荷文本；
   ``_chunk`` 信封的重组在传输层（``wing_probe.driver.ws``）完成，这里只见完整事件；
2. **游标**：``cursor`` 指向"尚未被断言消费"的第一个事件。命中即推进游标，
   从而把"必须看到 / 顺序"这类断言变成单调的读游标操作（无需"已断言"标记集）；
3. **等待**：``wait_for`` 是唯一的阻塞点——有界（``within``）、事件驱动（无 sleep
   轮询），且不推进游标（推进是断言原语的职责，见 ``wing_probe.watch.expect``）。

时间基准：``at`` 一律是"相对 env 启动的秒数"（``env.started_at`` 为原点，design D7），
所以时间线之间的时间戳可直接互相对照。
"""

from __future__ import annotations

import asyncio
import time
from collections import deque
from collections.abc import Callable, Iterable, Iterator, Mapping
from dataclasses import dataclass
from typing import Any, cast

Clock = Callable[[], float]
"""单调时钟（默认 ``time.monotonic``；测试可注入）。"""

Predicate = Callable[["Event"], bool]
"""事件谓词。"""

Where = Predicate | Mapping[str, Any]
"""过滤条件：谓词，或"字段子集相等"的映射（``{"tool_name": "Bash"}``）。"""

#: 原始帧日志的默认容量（行数与字节数双上限——时间线永久保留完整事件载荷，
#: 帧日志只是传输层诊断视图，因此有界）。
DEFAULT_MAX_FRAMES = 512
DEFAULT_MAX_FRAME_BYTES = 32 * 1024 * 1024


def normalize_types(value: str | Iterable[str]) -> tuple[str, ...]:
    """``str | Iterable[str]`` → tuple（保序，不排序、不去重）。"""
    items = (value,) if isinstance(value, str) else tuple(value)
    if not items:
        raise ValueError("at least one event type is required")
    for item in items:
        if not isinstance(item, str) or not item:
            raise ValueError(f"event types must be non-empty strings, got {item!r}")
    return items


def mapping_matches(data: Mapping[str, Any], spec: Mapping[str, Any]) -> bool:
    """字段子集相等（``spec`` 的每个键都存在于 ``data`` 且值相等）。"""
    return all(key in data and data[key] == expected for key, expected in spec.items())


def to_predicate(where: Where | None) -> Predicate | None:
    """把 ``Where`` 归一化成谓词（映射 = "字段子集相等"；``None`` = 不过滤）。"""
    if where is None:
        return None
    if isinstance(where, Mapping):
        spec = cast("Mapping[str, Any]", where)
        return lambda event: mapping_matches(event.data, spec)
    return cast("Predicate", where)


def matches(
    event: Event,
    types: tuple[str, ...] | None,
    where: Where | None = None,
) -> bool:
    """事件是否满足 type 集合与谓词（``types=None`` 表示不限类型）。"""
    if types is not None and event.type not in types:
        return False
    predicate = to_predicate(where)
    return True if predicate is None else predicate(event)


@dataclass(frozen=True, slots=True)
class Event:
    """时间线上的一个事件（WS 完整帧的应用层投影）。

    ``data`` 是帧里的原始 JSON 对象（不做类型化映射——design D2：防镜像漂移，
    断言直接读字段名）；``raw`` 是重组后的原始载荷文本（报告与现场转储的数据源）。
    """

    index: int
    """时间线内序号（0-based，等于事件在时间线中的下标）。"""
    type: str
    data: dict[str, Any]
    at: float
    """相对 env 启动的单调时钟（秒）。"""
    raw: str = ""
    frames: int = 1
    """传输层帧数（``> 1`` 表示由 ``_chunk`` 信封重组而来）。"""

    @property
    def session_id(self) -> str | None:
        value = self.data.get("session_id")
        return value if isinstance(value, str) else None

    @property
    def uuid(self) -> str | None:
        value = self.data.get("uuid")
        return value if isinstance(value, str) else None

    def get(self, key: str, default: Any = None) -> Any:
        return self.data.get(key, default)

    def __getitem__(self, key: str) -> Any:
        return self.data[key]

    def __contains__(self, key: str) -> bool:
        return key in self.data

    def matches(
        self, types: tuple[str, ...] | None, where: Where | None = None
    ) -> bool:
        return matches(self, types, where)

    def as_dict(self) -> dict[str, Any]:
        """现场转储用的 JSON 记录（``timeline.jsonl`` 的每一行）。"""
        return {
            "index": self.index,
            "at": round(self.at, 6),
            "type": self.type,
            "data": self.data,
            "frames": self.frames,
            "raw": self.raw,
        }

    def __str__(self) -> str:
        return f"#{self.index} {self.type} (+{self.at:.3f}s)"


@dataclass(frozen=True, slots=True)
class Frame:
    """一条原始 WS 帧（重组前，含 ``_chunk`` 信封帧）。"""

    at: float
    """相对 env 启动的单调时钟（秒）。"""
    text: str
    chunk: bool = False
    """是否为 ``_chunk`` 信封帧。"""

    @property
    def size(self) -> int:
        return len(self.text)


class FrameLog:
    """原始帧的有界环形日志（报告"原始帧尾部"与现场转储的数据源）。"""

    def __init__(
        self,
        *,
        max_frames: int = DEFAULT_MAX_FRAMES,
        max_bytes: int = DEFAULT_MAX_FRAME_BYTES,
    ) -> None:
        self._frames: deque[Frame] = deque()
        self._bytes = 0
        self._max_frames = max_frames
        self._max_bytes = max_bytes
        self.dropped = 0
        """因容量上限被丢弃的帧数（报告里如实标注）。"""

    def add(self, text: str, *, at: float, chunk: bool = False) -> Frame:
        frame = Frame(at=at, text=text, chunk=chunk)
        self._frames.append(frame)
        self._bytes += frame.size
        self._trim()
        return frame

    def _trim(self) -> None:
        while self._frames and (
            len(self._frames) > self._max_frames or self._bytes > self._max_bytes
        ):
            removed = self._frames.popleft()
            self._bytes -= removed.size
            self.dropped += 1

    @property
    def frames(self) -> list[Frame]:
        return list(self._frames)

    @property
    def byte_size(self) -> int:
        return self._bytes

    def tail(self, count: int) -> list[Frame]:
        if count <= 0:
            return []
        return list(self._frames)[-count:]

    def __len__(self) -> int:
        return len(self._frames)

    def __iter__(self) -> Iterator[Frame]:
        return iter(list(self._frames))


class Timeline:
    """事件缓冲 + 未消费游标（``Watcher`` 的状态底座）。

    写入方只有一个：WS 读任务（单事件循环、单任务顺序调用），因此 ``append``
    是同步的；等待方通过"版本化事件"被唤醒（append 时换一个新的 ``asyncio.Event``，
    所有挂起的等待者一起醒来重查谓词）。
    """

    def __init__(
        self,
        name: str = "timeline",
        *,
        clock: Clock = time.monotonic,
        started_at: float | None = None,
    ) -> None:
        self.name = name
        self._clock = clock
        self._started_at = clock() if started_at is None else started_at
        self._events: list[Event] = []
        self._cursor = 0
        self._changed = asyncio.Event()

    # ── 时间基准 ──────────────────────────────────────────────

    @property
    def started_at(self) -> float:
        """时间原点（``time.monotonic()`` 尺度，通常即 ``env.started_at``）。"""
        return self._started_at

    def now(self) -> float:
        """当前时刻（相对时间原点，秒）。"""
        return self._clock() - self._started_at

    # ── 写入 ──────────────────────────────────────────────────

    def append(
        self,
        type: str,
        data: Mapping[str, Any] | None = None,
        *,
        raw: str | None = None,
        at: float | None = None,
        frames: int = 1,
    ) -> Event:
        """追加一个事件并唤醒等待者（``at`` 为相对秒；None 表示"现在"）。"""
        event = Event(
            index=len(self._events),
            type=type,
            data=dict(data) if data is not None else {},
            at=self.now() if at is None else at,
            raw=raw if raw is not None else "",
            frames=frames,
        )
        self._events.append(event)
        self._wake()
        return event

    def _wake(self) -> None:
        """唤醒所有等待者：替换 ``asyncio.Event``（单循环内无丢失窗口）。"""
        changed, self._changed = self._changed, asyncio.Event()
        changed.set()

    # ── 游标 ──────────────────────────────────────────────────

    @property
    def cursor(self) -> int:
        """未消费游标（下一个待断言事件的序号）。"""
        return self._cursor

    def set_cursor(self, index: int) -> None:
        """设置游标（越界抛 ``ValueError``）——断言原语命中后推进它。"""
        if index < 0 or index > len(self._events):
            raise ValueError(
                f"cursor {index} out of range for timeline {self.name!r} "
                f"({len(self._events)} event(s))"
            )
        self._cursor = index

    def pending(self) -> list[Event]:
        """游标之后（含游标位置）的全部事件——报告"实际发生了什么"的来源。"""
        return self._events[self._cursor :]

    def consumed(self) -> list[Event]:
        """已被断言消费的事件（``assert_never``／排查用）。"""
        return self._events[: self._cursor]

    # ── 查询（不推进游标） ────────────────────────────────────

    def all(self) -> list[Event]:
        return list(self._events)

    def events(
        self,
        type: str | Iterable[str] | None = None,
        *,
        where: Where | None = None,
        since: int = 0,
    ) -> list[Event]:
        """全量查询：按 type 集合与谓词过滤，可选起点 ``since``（默认整条时间线）。"""
        types = None if type is None else normalize_types(type)
        return [
            event
            for event in self._events[max(since, 0) :]
            if matches(event, types, where)
        ]

    def types(self) -> list[str]:
        return [event.type for event in self._events]

    def counts(self) -> dict[str, int]:
        result: dict[str, int] = {}
        for event in self._events:
            result[event.type] = result.get(event.type, 0) + 1
        return result

    def __len__(self) -> int:
        return len(self._events)

    def __iter__(self) -> Iterator[Event]:
        return iter(list(self._events))

    # ── 等待 ──────────────────────────────────────────────────

    async def wait_for(
        self,
        predicate: Predicate,
        *,
        within: float,
        from_cursor: bool = True,
    ) -> Event | None:
        """等待首个满足 ``predicate`` 的事件（有界，``within`` 秒后返回 None）。

        不推进游标（那是断言原语的职责）；``within=0`` 退化为"只看当下已有"。
        永远不会有无限等待——调用方无法传 None。
        """
        if within < 0:
            raise ValueError(f"within must be >= 0, got {within}")
        deadline = self._clock() + within
        while True:
            for index in range(self._cursor if from_cursor else 0, len(self._events)):
                event = self._events[index]
                if predicate(event):
                    return event
            remaining = deadline - self._clock()
            if remaining <= 0:
                return None
            waiter = self._changed
            try:
                await asyncio.wait_for(waiter.wait(), remaining)
            except TimeoutError:
                return None
