"""唤醒场景：``done`` 的静息判据（多级编排 manager → scheduler → executor）。

被守的语义（`docs/dev/wake.md` §1.2 + `wing/wake/quiescence.py`）：

- ``done`` 是「**这一层不会再自己动了**」，不是「这一轮说完了」。中间层（scheduler）派完活
  就结束本轮时，只要子树里还有会话会动（executor 在跑 / 它自己还被别人欠着一次叫醒），
  这次收口就**不投递、不扣配额、保持武装**——否则被叫醒的上层只会看到"它刚派完"，
  而真正的终态因配额已被吃光再没有人叫醒它；
- 押住不是丢弃：中间层真的静息之后（executor 收口 → 那条叫醒线被消耗掉），下一次收口
  照常投递并终结。整条链**自底向上收敛**，不需要任何定时兜底；
- ``ask`` 完全不受判据约束：提问立即投递、不消耗 done 配额。

三个会话都是**真会话**（真跑模型），血缘经 ``POST /api/session/create`` 的
``parent_session_id`` 落到盘上，唤醒线经 ``POST /api/wake/create`` 登记——与 agent 在
shell 里派活时 CLI 做的事逐字一致。
"""

from __future__ import annotations

import asyncio
import time
from typing import Any

import pytest

from wing_probe import Probe, ToolCall, Turn
from wing_probe.driver.session import Session

MANAGER_MODEL = "probe/wake-settled-manager"
SCHEDULER_MODEL = "probe/wake-settled-scheduler"
EXECUTOR_MODEL = "probe/wake-settled-executor"

#: executor 的长回合（分片 2 字符 + 每片 50ms ≈ 5s）：撑开"中间层收口"那一段观察窗口。
LONG_TEXT = (
    "grinding through the long task one small step at a time so that the middle "
    "layer finishes its own turn while this one is unmistakably still running"
)

#: scheduler 给人类的那一问（多问题形态；信封里会带上 tool_call_id）。
QUESTION = {
    "id": "next",
    "header": "Next",
    "question": "Ship it?",
    "options": [{"label": "yes"}, {"label": "no"}],
}

ACK = "ack"


async def _create(probe: Probe, *, model: str, parent: str | None = None) -> Session:
    """带血缘建会话并挂上事件流（CLI 在 agent 的 shell 里就是这么建的）。"""
    body: dict[str, Any] = {
        "agent": {"model_id": model},
        "workspace": str(probe.workspace),
        # 带标 = 列表里始终可见（无名且无标的会话不进列表，状态断言会落空）
        "tags": ["wake-settled"],
    }
    if parent is not None:
        body["parent_session_id"] = parent
    created = await probe.driver_required.http.request(
        "POST", "/api/session/create", body=body
    )
    return await probe.driver_required.attach(created["session_id"])


async def _wake(probe: Probe, *, source: str, target: str) -> dict[str, Any]:
    created = await probe.driver_required.http.create_wake(
        kind="session", source_session_id=source, target_session_id=target
    )
    return created["wake"]


async def _wake_row(probe: Probe, wake_id: int) -> dict[str, Any]:
    payload = await probe.driver_required.http.list_wakes(all=True)
    for wake in payload["wakes"]:
        if wake["id"] == wake_id:
            return wake
    raise AssertionError(f"wake #{wake_id} disappeared: {payload['wakes']}")


async def _status(probe: Probe, session_id: str) -> str | None:
    """``/api/session/list`` 里的运行时状态。"""
    payload = await probe.driver_required.http.request("GET", "/api/session/list")
    for entry in payload.get("sessions", []):
        if entry.get("id") == session_id:
            return entry.get("status")
    return None


def _envelopes(probe: Probe, session_id: str) -> list[str]:
    """该会话收到的唤醒信封（内置信封都以 ``[wing]`` 开头）。"""
    return [
        message.get("content") or ""
        for message in probe.history(session_id).messages()
        if message["role"] == "user" and "[wing]" in (message.get("content") or "")
    ]


async def _wait_envelope(
    probe: Probe, session_id: str, needle: str, *, within: float = 30.0
) -> str:
    deadline = time.monotonic() + within
    while time.monotonic() < deadline:
        for content in _envelopes(probe, session_id):
            if needle in content:
                return content
        await asyncio.sleep(0.05)
    raise AssertionError(
        f"{needle!r} never reached {session_id}: {_envelopes(probe, session_id)}"
    )


async def _start_long_turn(executor: Session) -> None:
    """让 executor 跑起来但不阻塞``（派发方"派完就走"）``。"""
    await executor.send("long task")
    await executor.watch.expect("text", within=20)


@pytest.mark.probe_env(
    models=[MANAGER_MODEL, SCHEDULER_MODEL, EXECUTOR_MODEL],
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_a_middle_layer_with_live_work_does_not_wake_its_parent(
    probe: Probe,
) -> None:
    """scheduler 收口时 executor 还在跑：manager 这次不被叫醒，静息后才被叫醒。"""
    probe.register(EXECUTOR_MODEL, Turn.of(text=LONG_TEXT, chunk=2, delay=0.05))
    probe.register(
        SCHEDULER_MODEL,
        Turn.of(text="dispatched the executor"),
        Turn.of(text="integrated the result"),
    )
    probe.register(MANAGER_MODEL, *[Turn.of(text=ACK) for _ in range(4)])

    manager = await _create(probe, model=MANAGER_MODEL)
    scheduler = await _create(probe, model=SCHEDULER_MODEL, parent=manager.session_id)
    executor = await _create(probe, model=EXECUTOR_MODEL, parent=scheduler.session_id)

    # 两条叫醒线：executor → scheduler（子收口叫醒它）、scheduler → manager（本场景的被测线）
    await _wake(probe, source=executor.session_id, target=scheduler.session_id)
    to_manager = await _wake(
        probe, source=scheduler.session_id, target=manager.session_id
    )

    await _start_long_turn(executor)
    assert await _status(probe, executor.session_id) == "working"

    # scheduler 只是"派完活就结束本轮"：executor 仍在跑，这一次收口不该叫醒 manager
    assert (await scheduler.chat("dispatch the executor")).data["subtype"] == "success"
    await asyncio.sleep(1.0)

    assert _envelopes(probe, manager.session_id) == []
    held = await _wake_row(probe, to_manager["id"])
    assert (held["state"], held["fire_count"]) == ("active", 0), held
    # 观察窗口内前提始终成立（不是"executor 恰好已经跑完了"）
    assert await _status(probe, executor.session_id) == "working"

    # executor 收口 → 叫醒 scheduler（它是真的干完了）→ 这一次收口才叫醒 manager
    await executor.watch.expect("turn_result", within=40)
    envelope = await _wait_envelope(
        probe, manager.session_id, f"[wing] session {scheduler.session_id} finished"
    )
    assert "subtype=success" in envelope, envelope
    settled = await _wake_row(probe, to_manager["id"])
    assert (settled["state"], settled["fire_count"]) == ("done", 1), settled

    # 中间层确实被叫醒过：收敛是自底向上的，不是跳过了它
    middle = _envelopes(probe, scheduler.session_id)
    assert any(
        f"[wing] session {executor.session_id} finished" in text for text in middle
    ), middle


@pytest.mark.probe_env(
    models=[MANAGER_MODEL, SCHEDULER_MODEL, EXECUTOR_MODEL],
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_ask_is_delivered_while_the_subtree_is_still_running(
    probe: Probe,
) -> None:
    """提问不受静息判据约束：子树还在跑，ask 也立刻送达，且不消耗 done 配额。"""
    probe.register(EXECUTOR_MODEL, Turn.of(text=LONG_TEXT, chunk=2, delay=0.05))
    probe.register(
        SCHEDULER_MODEL,
        Turn.of(tool_calls=[ToolCall("AskUserQuestion", {"questions": [QUESTION]})]),
    )
    probe.register(MANAGER_MODEL, *[Turn.of(text=ACK) for _ in range(4)])

    manager = await _create(probe, model=MANAGER_MODEL)
    scheduler = await _create(probe, model=SCHEDULER_MODEL, parent=manager.session_id)
    executor = await _create(probe, model=EXECUTOR_MODEL, parent=scheduler.session_id)

    await _wake(probe, source=executor.session_id, target=scheduler.session_id)
    to_manager = await _wake(
        probe, source=scheduler.session_id, target=manager.session_id
    )

    await _start_long_turn(executor)
    assert await _status(probe, executor.session_id) == "working"

    await scheduler.send("ask the human")
    envelope = await _wait_envelope(
        probe, manager.session_id, "is waiting for your answer"
    )
    assert "tool_call_id=" in envelope, envelope
    assert "Ship it?" in envelope, envelope

    # 送达了一次 ask，但 done 配额（缺省 1）一次都没被消耗：触发线仍在武装
    row = await _wake_row(probe, to_manager["id"])
    assert (row["state"], row["fire_count"]) == ("active", 1), row
    # 前提成立：ask 到达时子树还在跑（判据根本没参与）
    assert await _status(probe, executor.session_id) == "working"
