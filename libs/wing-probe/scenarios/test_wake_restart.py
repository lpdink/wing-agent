"""唤醒场景 4：重启恢复（错过补偿的两条分支）。

被守的语义（`WakeService.recover_missed`，只靠 `wing.db` 的排期恢复，进程内状态全丢）：

- **一次性**（`in:1s`）：停机窗口里错过的排期 → 新进程启动时**补发一次**，然后 `done`；
- **周期型**（`every:5s`）：错过的排期**不补发**，从启动时刻重新对齐下一次（错过的 tick 一律
  不追），之后照常触发一次（`#1`）；
- 一条边都不漏、也不追债式连发（周期型的 `fire_count` 在重启后必须是 0，直到重新对齐的
  那一次真的到点）。

停机用 `kill -9`（`probe.env.kill_gateway()`）：优雅停机的话进程会在收尾前把到期的触发
发掉，"错过"这个现场就造不出来。重启后旧句柄不再收事件——断言锚在 `/api/wake/list` 与
落盘的 `history.jsonl` 上（与真实重启的用户路径一致）。
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable
from typing import Any

import pytest

from wing_probe import Probe, Turn

PARENT_MODEL = "probe/wake-restart-parent"

PARENT_ACK = "ack"
ONCE_PROMPT = "compensate me"
TICK_PROMPT = "tick"

#: 停机时长：必须 > 一次性 trigger 的 1s 排期（否则它还没到期，谈不上"错过"）。
DOWN_SECONDS = 2.5

#: 周期型间隔：要足够长，让「重启后立刻查一次」与「重新对齐后真的触发」两件事不重叠。
TICK_SPEC = "every:5s"


def _envelope_text(message: dict[str, Any]) -> str:
    return message.get("content") or ""


async def _wake_row(probe: Probe, wake_id: int) -> dict[str, Any]:
    payload = await probe.driver_required.http.list_wakes(all=True)
    for wake in payload["wakes"]:
        if wake["id"] == wake_id:
            return wake
    raise AssertionError(f"wake #{wake_id} disappeared: {payload['wakes']}")


async def _until(
    probe: Probe,
    wake_id: int,
    *,
    check: Callable[[dict[str, Any]], bool],
    within: float = 25.0,
) -> dict[str, Any]:
    deadline = time.monotonic() + within
    row: dict[str, Any] = {}
    while time.monotonic() < deadline:
        row = await _wake_row(probe, wake_id)
        if check(row):
            return row
        await asyncio.sleep(0.05)
    raise AssertionError(f"wake #{wake_id} never matched: {row}")


async def _wait_history(
    probe: Probe, session_id: str, needle: str, *, within: float = 20.0
) -> dict[str, Any]:
    deadline = time.monotonic() + within
    messages: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        messages = probe.history(session_id).messages()
        for message in messages:
            if message["role"] == "user" and needle in _envelope_text(message):
                return message
        await asyncio.sleep(0.05)
    raise AssertionError(f"{needle!r} never landed in {session_id}: {messages}")


@pytest.mark.probe_env(models=[PARENT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_restart_compensates_once_and_realigns_recurring(probe: Probe) -> None:
    """一次性补发一次；周期型只重新对齐、不补发，之后照常触发。"""
    probe.register(PARENT_MODEL, *[Turn.of(text=PARENT_ACK) for _ in range(12)])
    parent = await probe.session(model=PARENT_MODEL)
    http = probe.driver_required.http  # 重启会换 driver：之后一律重新取

    once = (
        await http.create_wake(
            kind="timer",
            target_session_id=parent.session_id,
            spec="in:1s",
            prompt=ONCE_PROMPT,
        )
    )["wake"]
    tick = (
        await http.create_wake(
            kind="timer",
            target_session_id=parent.session_id,
            spec=TICK_SPEC,
            prompt=TICK_PROMPT,
            times=3,
        )
    )["wake"]

    # 硬杀：两条都还没到期（1s / 5s），收尾一行都没跑
    await probe.env.kill_gateway()
    await asyncio.sleep(DOWN_SECONDS)
    await probe.restart_gateway()

    # ── 周期型：启动时只重新对齐，不补发（fire_count 必须还是 0）──
    row = await _wake_row(probe, tick["id"])
    assert row["state"] == "active", row
    assert row["fire_count"] == 0, row
    assert row["next_fire_at"] > row["created_at"], row

    # ── 一次性：补发一次并终结 ──
    comp = await _until(probe, once["id"], check=lambda r: r["state"] == "done")
    assert comp["fire_count"] == 1, comp

    message = await _wait_history(
        probe, parent.session_id, "[wing] scheduled wake (in:1s, #1)"
    )
    assert ONCE_PROMPT in _envelope_text(message), message

    # ── 周期型在重新对齐到点后照常触发（#1，只一次）──
    fired = await _until(probe, tick["id"], check=lambda r: r["fire_count"] >= 1)
    assert fired["fire_count"] == 1, fired  # 不追债：错过的 tick 没有连发
    await _wait_history(
        probe, parent.session_id, f"[wing] scheduled wake ({TICK_SPEC}, #1)"
    )

    # 取消之后不再触发（重启后的调度循环确实在按库里的排期跑）
    assert (await probe.driver_required.http.cancel_wake(tick["id"]))[
        "canceled"
    ] is True
    stopped = await _wake_row(probe, tick["id"])
    assert stopped["state"] == "canceled", stopped
    await asyncio.sleep(1.0)
    assert (await _wake_row(probe, tick["id"]))["fire_count"] == stopped["fire_count"]
