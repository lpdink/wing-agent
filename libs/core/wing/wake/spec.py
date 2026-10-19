"""wing/wake/spec.py — 时间 spec 的语法与推进（纯函数）。

spec 是 ``wake_trigger.spec`` 列的**唯一语法**（库里存字符串，不存计算后的时刻）：

===========  ==================  ======  ==============================
CLI          spec                次数    首轮
===========  ==================  ======  ==============================
``--in 30m``  ``in:30m``         1       now+30m
``--at t``    ``at:<local iso>``  1       该时刻
``--every d`` ``every:30m``       ∞ / N   now+30m（不按墙钟对齐）
``--daily h`` ``daily:07:00``     ∞ / N   下一个本地 hh:mm（已过则明天）
===========  ==================  ======  ==============================

两条不变量：

  - **推进一律从 now 起算**（``advance_fire_at_ms``）：网关慢一拍、机器休眠、进程重启之后都不会
    「补发一串」——错过的 tick 一律不追。
  - **时区 = 网关宿主本地时区**；对外的时刻一律是 epoch 毫秒（与时区无关），只有 ``at:`` / ``daily:``
    的解析要在本地时区里做一次换算。
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import datetime, timedelta

#: 时长单位 → 秒。``d`` 是 86400 秒（本地「天」的粗粒度，不处理夏令时跳变——
#: 一次性 / 周期定时器都用「秒」这个绝对量，见 ``every``）。
_DURATION_UNITS: dict[str, int] = {"s": 1, "m": 60, "h": 3600, "d": 86400}

#: 单个时长分量 ``12h``；组合形 ``1h30m`` = 多个分量首尾相接。
_DURATION_PART = re.compile(r"(\d+)\s*([smhd])")

#: ``daily:`` 的墙钟时刻。
_DAILY_TIME = re.compile(r"^([01]?\d|2[0-3]):([0-5]\d)$")

#: 一次性 spec（``in`` / ``at``）：触发一次即终结，配额恒为 1。
ONESHOT_KINDS: frozenset[str] = frozenset({"in", "at"})

#: 合法 spec 的形状提示（错误文案复用，CLI / HTTP / 文档三处一致）。
SPEC_HINT = "in:30m | at:2026-03-01T09:00 | every:30m | daily:07:00"


class SpecError(ValueError):
    """spec 语法非法（HTTP 400 / CLI 用法错误）。"""


@dataclass(frozen=True)
class WakeSpec:
    """解析后的 spec。

    ``kind`` 是四选一；``duration_seconds`` 只对 ``in`` / ``every`` 有意义，
    ``instant_ms`` 只对 ``at`` 有意义，``daily`` 只带 ``daily_hhmm``（``(h, m)``）。
    """

    kind: str
    duration_seconds: int | None = None
    instant_ms: int | None = None
    daily_hhmm: tuple[int, int] | None = None

    @property
    def one_shot(self) -> bool:
        """一次性 spec（``in`` / ``at``）——配额恒为 1，触发即终结。"""
        return self.kind in ONESHOT_KINDS

    @property
    def text(self) -> str:
        """规范化后的 spec 文本（写库 / `--list` 回显 / timer 信封里的出处行）。

        时长走 :func:`format_duration`：`1h30m` 与 `90m` 都落成 `1h30m`——同一段时间
        只有一种写法（用户不会在回执里看到自己没输入过的 `90m`）。
        """
        if self.kind in ("in", "every"):
            assert self.duration_seconds is not None
            return f"{self.kind}:{format_duration(self.duration_seconds)}"
        if self.kind == "at":
            assert self.instant_ms is not None
            return f"at:{datetime.fromtimestamp(self.instant_ms / 1000).isoformat()}"
        assert self.daily_hhmm is not None
        hour, minute = self.daily_hhmm
        return f"daily:{hour:02d}:{minute:02d}"


# ============================================================
# 时长
# ============================================================


def parse_duration(text: str) -> int:
    """``30s`` / ``10m`` / ``2h`` / ``1d`` / ``1h30m`` → 秒。

    Raises:
        SpecError: 空串、未知单位、只有空白、尾随垃圾（``10m!``）、总量为 0
    """
    raw = (text or "").strip()
    if not raw:
        raise SpecError("empty duration (expected e.g. 30s / 10m / 2h / 1d)")
    total = 0
    pos = 0
    for match in _DURATION_PART.finditer(raw):
        if match.start() != pos:
            raise SpecError(
                f"invalid duration {text!r} (expected e.g. 30s / 10m / 2h / 1d / 1h30m)"
            )
        total += int(match.group(1)) * _DURATION_UNITS[match.group(2)]
        pos = match.end()
    if pos != len(raw):
        raise SpecError(
            f"invalid duration {text!r} (expected e.g. 30s / 10m / 2h / 1d / 1h30m)"
        )
    if total <= 0:
        raise SpecError(f"duration must be > 0, got {text!r}")
    return total


def format_duration(seconds: int) -> str:
    """秒 → 紧凑记法（``5400`` → ``1h30m``，``90000`` → ``1d1h``）。"""
    remaining = int(seconds)
    parts: list[str] = []
    for unit in ("d", "h", "m", "s"):
        size = _DURATION_UNITS[unit]
        amount, remaining = divmod(remaining, size)
        if amount:
            parts.append(f"{amount}{unit}")
    return "".join(parts) or "0s"


# ============================================================
# spec
# ============================================================


def parse_spec(text: str) -> WakeSpec:
    """spec 文本 → :class:`WakeSpec`。

    Raises:
        SpecError: 未知前缀 / 形态非法 / 时刻解析不了 / 时长非法
    """
    raw = (text or "").strip()
    prefix, sep, body = raw.partition(":")
    if not sep or not body.strip():
        raise SpecError(f"invalid wake spec {text!r} (expected {SPEC_HINT})")
    kind = prefix.strip()
    body = body.strip()

    if kind in ("in", "every"):
        seconds = parse_duration(body)
        return WakeSpec(kind=kind, duration_seconds=seconds)
    if kind == "at":
        return WakeSpec(kind=kind, instant_ms=_parse_at_ms(body))
    if kind == "daily":
        match = _DAILY_TIME.match(body)
        if match is None:
            raise SpecError(f"invalid daily time {body!r} (expected HH:MM, 24h)")
        return WakeSpec(
            kind=kind, daily_hhmm=(int(match.group(1)), int(match.group(2)))
        )
    raise SpecError(f"unknown wake spec kind {kind!r} (expected {SPEC_HINT})")


def _parse_at_ms(text: str) -> int:
    """``at:`` 的时刻 → epoch 毫秒（本地时区；``T`` 与空格都是合法分隔符）。"""
    try:
        instant = datetime.fromisoformat(text)
    except ValueError as exc:
        raise SpecError(
            f"invalid at time {text!r} (expected local ISO, e.g. 2026-03-01T09:00)"
        ) from exc
    return int(instant.timestamp() * 1000)


def first_fire_at_ms(spec: WakeSpec, now_ms: int) -> int:
    """创建期的首轮触发时刻。"""
    if spec.kind == "in":
        assert spec.duration_seconds is not None
        return now_ms + spec.duration_seconds * 1000
    if spec.kind == "at":
        assert spec.instant_ms is not None
        return spec.instant_ms
    return advance_fire_at_ms(spec, now_ms)


def advance_fire_at_ms(spec: WakeSpec, now_ms: int) -> int:
    """触发之后（以及周期型重启对齐）的下一次触发时刻——**一律从 now 起算**。

    ``every``：now + 间隔（不按墙钟对齐；慢一拍不补偿）。
    ``daily``：now 之后的下一个本地 hh:mm（当天的还没过就是今天，否则明天）。
    一次性 spec 没有下一轮——返回 ``now_ms``（调用方不会用到它：一次性触发即终结）。
    """
    if spec.kind == "every":
        assert spec.duration_seconds is not None
        return now_ms + spec.duration_seconds * 1000
    if spec.kind == "daily":
        assert spec.daily_hhmm is not None
        hour, minute = spec.daily_hhmm
        now = datetime.fromtimestamp(now_ms / 1000)
        target = now.replace(hour=hour, minute=minute, second=0, microsecond=0)
        if target <= now:
            target += timedelta(days=1)
        return int(target.timestamp() * 1000)
    return now_ms
