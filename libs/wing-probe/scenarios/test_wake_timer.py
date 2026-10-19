"""唤醒场景 3：定时触发到点投递 / 取消后不投递。

被守的语义（`WakeScheduler` + `wake_trigger` + `/api/wake/*`）：

- `kind=timer` + `spec=in:1s`：到点后 target 收到 timer 信封
  `[wing] scheduled wake (in:1s, #1)` + prompt 原文；一次性 spec 触发即终结（`state=done`）；
- 另一条 `in:2s` 在到期前被 `POST /api/wake/cancel` 取消 → `state=canceled`，
  **到期也不投递**（`fire_count` 保持 0），父会话只收到前一条；
- 取消是幂等的：第二次取消回 `canceled=false`；不存在的 id 回 404。
"""

from __future__ import annotations

import asyncio
import time
from typing import Any

import pytest

from wing_probe import Probe, Turn
from wing_probe.watch import Event

PARENT_MODEL = "probe/wake-timer-parent"

PARENT_ACK = "ack"
FIRE_PROMPT = "ping the build"
CANCELED_PROMPT = "should never arrive"


def _envelope_text(event: Event) -> str:
    return event.data.get("content") or ""


async def _wait_wake(
    probe: Probe, wake_id: int, *, state: str, within: float = 20.0
) -> dict[str, Any]:
    deadline = time.monotonic() + within
    last: dict[str, Any] = {}
    while time.monotonic() < deadline:
        payload = await probe.driver_required.http.list_wakes(all=True)
        for wake in payload["wakes"]:
            if wake["id"] == wake_id:
                last = wake
                if wake["state"] == state:
                    return wake
        await asyncio.sleep(0.05)
    raise AssertionError(f"wake #{wake_id} never reached state={state!r}: {last}")


@pytest.mark.probe_env(models=[PARENT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_timer_fires_and_cancel_does_not(probe: Probe) -> None:
    """到点投递一次；取消掉的那条即使到期也不投。"""
    probe.register(PARENT_MODEL, *[Turn.of(text=PARENT_ACK) for _ in range(8)])
    parent = await probe.session(model=PARENT_MODEL)
    http = probe.driver_required.http

    fired = (
        await http.create_wake(
            kind="timer",
            target_session_id=parent.session_id,
            spec="in:1s",
            prompt=FIRE_PROMPT,
        )
    )["wake"]
    canceled = (
        await http.create_wake(
            kind="timer",
            target_session_id=parent.session_id,
            spec="in:2s",
            prompt=CANCELED_PROMPT,
        )
    )["wake"]
    assert fired["spec"] == "in:1s" and fired["next_fire_at"], fired
    assert canceled["spec"] == "in:2s", canceled

    # 到期前取消（2s 的窗口足够宽：创建到取消是两次 HTTP）
    assert (await http.cancel_wake(canceled["id"]))["canceled"] is True
    assert (await http.cancel_wake(canceled["id"]))["canceled"] is False  # 幂等

    event = await parent.watch.expect(
        "user_message_accepted",
        where=lambda e: "scheduled wake" in _envelope_text(e),
        within=20,
    )
    envelope = _envelope_text(event)
    assert envelope == f"[wing] scheduled wake (in:1s, #1)\n\n{FIRE_PROMPT}", envelope

    done = await _wait_wake(probe, fired["id"], state="done")
    assert done["fire_count"] == 1, done
    assert done["last_fire_at"], done
    # 一次性 spec 终结：active 列表里它不在了
    assert [w["id"] for w in (await http.list_wakes())["wakes"]] == []

    # 等过被取消那条的到期时刻：它一次都没投
    await asyncio.sleep(2.5)
    everything = {w["id"]: w for w in (await http.list_wakes(all=True))["wakes"]}
    assert everything[canceled["id"]]["state"] == "canceled", everything[canceled["id"]]
    assert everything[canceled["id"]]["fire_count"] == 0, everything[canceled["id"]]
    assert everything[fired["id"]]["fire_count"] == 1, everything[fired["id"]]

    messages = probe.history(parent).messages()
    wake_messages = [
        message["content"]
        for message in messages
        if message["role"] == "user"
        and "scheduled wake" in (message.get("content") or "")
    ]
    assert len(wake_messages) == 1, wake_messages
    assert CANCELED_PROMPT not in wake_messages[0], wake_messages[0]


@pytest.mark.probe_env(models=[PARENT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_cancel_unknown_id_is_404_and_bad_spec_is_400(probe: Probe) -> None:
    """取消不存在的 id → 404；非法 spec → 400（错误面是契约的一部分）。"""
    from wing_probe.driver import DriverHttpError

    probe.register(PARENT_MODEL, Turn.of(text=PARENT_ACK))
    parent = await probe.session(model=PARENT_MODEL)
    http = probe.driver_required.http

    with pytest.raises(DriverHttpError) as missing:
        await http.cancel_wake(4242)
    assert missing.value.call.status == 404, missing.value.call.status

    with pytest.raises(DriverHttpError) as bad_spec:
        await http.create_wake(
            kind="timer",
            target_session_id=parent.session_id,
            spec="every:soon",
            prompt="x",
        )
    assert bad_spec.value.call.status == 400, bad_spec.value.call.status

    with pytest.raises(DriverHttpError) as self_wake:
        await http.create_wake(
            kind="session",
            target_session_id=parent.session_id,
            source_session_id=parent.session_id,
        )
    assert self_wake.value.call.status == 400, self_wake.value.call.status
