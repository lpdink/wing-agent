"""wing/wake — 会话唤醒：触发器注册 + 事件/时间驱动 + 内置信封投递。

两块驱动、一个出口：

- **事件驱动**：订阅 ``TurnResultEvent``（source 收口）/ ``AskEvent``（source 提问），把一条普通用户
  消息投回 target 会话（``runtime.post``，带新 ``request_id``）；``done`` 只在 source **静息**
  时投递（它还会自己再动就押住，见 ``quiescence.py``），``ask`` 永远立即投递；
- **时间驱动**：``WakeScheduler`` 睡到最近的 ``next_fire_at_ms``，到点投 ``timer`` 信封（心跳 / 定时
  唤醒），重启时对错过的排期做一次补偿；
- **出口**：投递与那条运行时事实（会话还会不会再跑一轮）都由组合根注入
  （:class:`wing.wake.ports.WakePorts`）——本包不 import ``runtime`` / ``gateway``，也不持有会话对象。

持久化在进程级 ``wing.db`` 的 ``wake_trigger`` 表（migration v2）；库打不开则整体不武装、网关照常跑。

产品语义、时间语义、信封与已知边界见 ``docs/dev/wake.md``。
"""

from __future__ import annotations

from wing.wake.envelope import ask_envelope, done_envelope, timer_envelope
from wing.wake.model import (
    DEFAULT_ON_EVENTS,
    EVENT_ASK,
    EVENT_DONE,
    KIND_SESSION,
    KIND_TIMER,
    STATE_ACTIVE,
    STATE_CANCELED,
    STATE_DONE,
    STATE_FAILED,
    WakeTrigger,
    parse_on_events,
)
from wing.wake.ports import WakePorts
from wing.wake.service import CreatedWake, WakeService, WakeUnavailableError
from wing.wake.spec import (
    SpecError,
    WakeSpec,
    advance_fire_at_ms,
    first_fire_at_ms,
    format_duration,
    parse_duration,
    parse_spec,
)

__all__ = [
    "DEFAULT_ON_EVENTS",
    "EVENT_ASK",
    "EVENT_DONE",
    "KIND_SESSION",
    "KIND_TIMER",
    "CreatedWake",
    "STATE_ACTIVE",
    "STATE_CANCELED",
    "STATE_DONE",
    "STATE_FAILED",
    "SpecError",
    "WakePorts",
    "WakeService",
    "WakeSpec",
    "WakeTrigger",
    "WakeUnavailableError",
    "advance_fire_at_ms",
    "ask_envelope",
    "done_envelope",
    "first_fire_at_ms",
    "format_duration",
    "parse_duration",
    "parse_on_events",
    "parse_spec",
    "timer_envelope",
]
