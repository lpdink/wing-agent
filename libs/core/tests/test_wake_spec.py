"""tests/test_wake_spec.py — 唤醒时间 spec 的语法与推进（纯函数）。

覆盖：
  - 时长解析（``30s`` / ``10m`` / ``2h`` / ``1d`` / 组合 ``1h30m``）与全部非法形态；
  - 四种 spec（in / at / every / daily）的解析与规范化文本；
  - 首轮时刻（``in`` = now+δ、``at`` = 该时刻、``daily`` = 下一个本地 HH:MM）；
  - 推进（``every`` 从 now 起算、``daily`` 跨天、正好到点算「已过」）。

时间只经参数进出（注入 now），不读时钟、不 sleep。
"""

from __future__ import annotations

from datetime import datetime, timedelta

import pytest

from wing.wake.spec import (
    SpecError,
    advance_fire_at_ms,
    first_fire_at_ms,
    format_duration,
    parse_duration,
    parse_spec,
)


def _ms(local: datetime) -> int:
    """本地 naive datetime → epoch 毫秒（与产品同一口径：naive = 本地时区）。"""
    return int(local.timestamp() * 1000)


# ============================================================
# 时长
# ============================================================


class TestDuration:
    @pytest.mark.parametrize(
        ("text", "expected"),
        [
            ("30s", 30),
            ("10m", 600),
            ("2h", 7200),
            ("1d", 86400),
            ("1h30m", 5400),
            ("1d2h3m4s", 86400 + 7200 + 180 + 4),
            ("  5m  ", 300),
            ("01h", 3600),
        ],
    )
    def test_valid_durations(self, text: str, expected: int) -> None:
        assert parse_duration(text) == expected

    @pytest.mark.parametrize(
        "text",
        ["", "   ", "10", "10x", "5m!", "0s", "0m", "-1m", "m", "10m5", "1m2x"],
    )
    def test_invalid_durations(self, text: str) -> None:
        with pytest.raises(SpecError):
            parse_duration(text)

    @pytest.mark.parametrize(
        ("seconds", "text"),
        [(30, "30s"), (600, "10m"), (7200, "2h"), (86400, "1d"), (5400, "1h30m")],
    )
    def test_format_duration(self, seconds: int, text: str) -> None:
        assert format_duration(seconds) == text


# ============================================================
# spec
# ============================================================


class TestParseSpec:
    def test_in(self) -> None:
        spec = parse_spec("in:30m")
        assert spec.kind == "in"
        assert spec.duration_seconds == 1800
        assert spec.one_shot
        assert spec.text == "in:30m"

    def test_every(self) -> None:
        spec = parse_spec("every:30m")
        assert spec.kind == "every"
        assert spec.duration_seconds == 1800
        assert not spec.one_shot
        assert spec.text == "every:30m"

    def test_at_is_local_iso(self) -> None:
        spec = parse_spec("at:2026-03-01 09:00")
        assert spec.kind == "at"
        assert spec.one_shot
        assert spec.instant_ms == _ms(datetime(2026, 3, 1, 9, 0))
        assert spec.text == "at:2026-03-01T09:00:00"

    def test_daily(self) -> None:
        spec = parse_spec("daily:07:00")
        assert spec.kind == "daily"
        assert spec.daily_hhmm == (7, 0)
        assert not spec.one_shot
        assert spec.text == "daily:07:00"

    def test_daily_accepts_single_digit_hour(self) -> None:
        assert parse_spec("daily:7:05").daily_hhmm == (7, 5)
        assert parse_spec("daily:7:05").text == "daily:07:05"

    @pytest.mark.parametrize(
        "text",
        [
            "",
            "30m",
            "in:",
            "in:bogus",
            "every:0m",
            "at:not-a-time",
            "daily:24:00",
            "daily:7:60",
            "daily:0700",
            "hourly:5m",
        ],
    )
    def test_invalid_specs(self, text: str) -> None:
        with pytest.raises(SpecError):
            parse_spec(text)


# ============================================================
# 首轮 / 推进
# ============================================================


class TestFirstFire:
    NOW = _ms(datetime(2026, 3, 1, 12, 0))

    def test_in_is_now_plus_duration(self) -> None:
        assert first_fire_at_ms(parse_spec("in:30m"), self.NOW) == self.NOW + 1_800_000

    def test_at_is_the_instant(self) -> None:
        spec = parse_spec("at:2026-03-01 18:30")
        assert first_fire_at_ms(spec, self.NOW) == _ms(datetime(2026, 3, 1, 18, 30))

    def test_at_in_the_past_is_kept(self) -> None:
        """指向过去的 ``at`` 不报错：调度器下一 tick 立即触发（等价「立刻」）。"""
        spec = parse_spec("at:2026-03-01 09:00")
        assert first_fire_at_ms(spec, self.NOW) < self.NOW

    def test_every_is_now_plus_duration(self) -> None:
        spec = parse_spec("every:2h")
        assert first_fire_at_ms(spec, self.NOW) == self.NOW + 7_200_000

    def test_daily_later_today(self) -> None:
        spec = parse_spec("daily:18:00")
        assert first_fire_at_ms(spec, self.NOW) == _ms(datetime(2026, 3, 1, 18, 0))

    def test_daily_already_passed_is_tomorrow(self) -> None:
        spec = parse_spec("daily:07:00")
        assert first_fire_at_ms(spec, self.NOW) == _ms(datetime(2026, 3, 2, 7, 0))

    def test_daily_exactly_now_is_tomorrow(self) -> None:
        """正好到点算「已过」——否则调度器会在同一 tick 里立刻又触发一次。"""
        now = _ms(datetime(2026, 3, 1, 7, 0))
        spec = parse_spec("daily:07:00")
        assert first_fire_at_ms(spec, now) == _ms(datetime(2026, 3, 2, 7, 0))


class TestAdvance:
    NOW = _ms(datetime(2026, 3, 1, 23, 0))

    def test_every_advances_from_now(self) -> None:
        """从 now 起算（不按调度时刻累加）：慢一拍不会连发。"""
        spec = parse_spec("every:30m")
        assert advance_fire_at_ms(spec, self.NOW) == self.NOW + 1_800_000

    def test_daily_crosses_midnight(self) -> None:
        spec = parse_spec("daily:07:00")
        assert advance_fire_at_ms(spec, self.NOW) == _ms(datetime(2026, 3, 2, 7, 0))

    def test_one_shot_has_no_next(self) -> None:
        spec = parse_spec("in:5m")
        assert advance_fire_at_ms(spec, self.NOW) == self.NOW

    def test_advance_uses_a_whole_day(self) -> None:
        spec = parse_spec("every:1d")
        assert advance_fire_at_ms(spec, self.NOW) == self.NOW + 86_400_000
        assert advance_fire_at_ms(spec, self.NOW) == _ms(
            datetime(2026, 3, 1, 23, 0) + timedelta(days=1)
        )
