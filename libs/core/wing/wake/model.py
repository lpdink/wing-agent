"""wing/wake/model.py — 触发器的领域形状（行 ↔ 对象）。

状态机是封闭的四值：``active`` → ``done`` / ``canceled`` / ``failed``，三个终态都不再武装、
不再重试（详见 ``docs/dev/wake.md``）。这里只放常量与映射，没有 I/O。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Literal

WakeKind = Literal["session", "timer"]

#: 触发器种类：``session`` 由事件驱动（订阅 source 会话），``timer`` 由时间驱动。
KIND_SESSION: WakeKind = "session"
KIND_TIMER: WakeKind = "timer"
KINDS: frozenset[str] = frozenset({KIND_SESSION, KIND_TIMER})

#: 触发器状态：``active`` 是唯一的非终态（done / canceled / failed 都不再武装、不再重试）。
STATE_ACTIVE = "active"
STATE_DONE = "done"
STATE_CANCELED = "canceled"
STATE_FAILED = "failed"

#: 事件种类（``on_events`` 的取值域；也是 ``TurnResultEvent`` / ``AskEvent`` 的简称）。
EVENT_DONE = "done"
EVENT_ASK = "ask"
EVENT_KINDS: frozenset[str] = frozenset({EVENT_DONE, EVENT_ASK})

#: ``on_events`` 的**规范顺序**（身份的一部分：同一组事件只有一种写法）。
EVENT_ORDER: tuple[str, ...] = (EVENT_DONE, EVENT_ASK)

#: ``on_events`` 列的缺省值（表定义里的 DEFAULT，同样在这里落地一次）。
DEFAULT_ON_EVENTS = ",".join(EVENT_ORDER)


def parse_on_events(text: str | None) -> str:
    """``done,ask`` 逗号列表 → **规范形**文本（固定顺序：先 ``done`` 后 ``ask``）。

    顺序由这里唯一决定，不跟用户输入走：``on_events`` 是会话型订阅的**身份**之一
    （``WakeStore.find_active_session`` 按字符串相等匹配），``ask,done`` 与 ``done,ask``
    必须落到同一行——否则同一个收口会被两条订阅各投一次。

    Raises:
        ValueError: 空 / 含未知值（``done,error``）/ 只有分隔符
    """
    raw = (text or "").strip()
    if not raw:
        raise ValueError(
            "on_events must not be empty (expected a comma list of: done,ask)"
        )
    kinds: list[str] = []
    for part in raw.split(","):
        name = part.strip().lower()
        if not name:
            continue
        if name not in EVENT_KINDS:
            raise ValueError(
                f"unknown on_events value {name!r} (expected a comma list of: done,ask)"
            )
        if name not in kinds:
            kinds.append(name)
    if not kinds:
        raise ValueError(
            "on_events must not be empty (expected a comma list of: done,ask)"
        )
    # 规范形：只在取值域里**存在**的才写进去，顺序固定（EVENT_ORDER 是唯一顺序来源）。
    return ",".join(kind for kind in EVENT_ORDER if kind in kinds)


@dataclass(frozen=True)
class WakeTrigger:
    """一条唤醒登记（``wake_trigger`` 行）。

    时间字段一律 epoch 毫秒（``None`` = 该 kind 不用它）：``created_at_ms`` / ``last_fire_at_ms`` /
    ``next_fire_at_ms``。``remaining_done`` 的语义按 ``kind`` 分岔（session = done 配额，
    timer = 剩余触发数），见 ``docs/dev/wake.md``。
    """

    id: int
    kind: str
    target_session_id: str
    source_session_id: str | None
    on_events: str
    spec: str | None
    next_fire_at_ms: int | None
    remaining_done: int | None
    prompt: str | None
    state: str
    fire_count: int
    last_fire_at_ms: int | None
    last_error: str | None
    created_at_ms: int
    created_by: str | None

    @property
    def active(self) -> bool:
        return self.state == STATE_ACTIVE

    @property
    def events(self) -> frozenset[str]:
        """``on_events`` 的集合形态（分发判定用）。"""
        return frozenset(part for part in self.on_events.split(",") if part)

    @classmethod
    def from_row(cls, row: Any) -> WakeTrigger:
        """``sqlite3.Row`` → 领域对象（列名即字段名，缺列会立刻抛出来）。"""
        return cls(
            id=int(row["id"]),
            kind=str(row["kind"]),
            target_session_id=str(row["target_session_id"]),
            source_session_id=row["source_session_id"],
            on_events=str(row["on_events"]),
            spec=row["spec"],
            next_fire_at_ms=row["next_fire_at_ms"],
            remaining_done=row["remaining_done"],
            prompt=row["prompt"],
            state=str(row["state"]),
            fire_count=int(row["fire_count"]),
            last_fire_at_ms=row["last_fire_at_ms"],
            last_error=row["last_error"],
            created_at_ms=int(row["created_at_ms"]),
            created_by=row["created_by"],
        )
