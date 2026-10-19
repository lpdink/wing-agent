"""唤醒场景 2：子会话提问 → 父会话收到 ask 信封 → 父回答 → 子收口 → 父再收到 done。

被守的语义：

- `AskEvent` 唤醒 target（信封带 `tool_call_id` + 选项），**不消耗 done 配额**——所以
  子会话被回答后收口时，同一条触发器还能再送一次 done 信封；
- 信封里的 `tool_call_id` 是**可行动**的地址：照着它 `wing run -r <sid> --tool-call-id`
  （探针走 `/api/session/send`）能把答案定向回那一个 waiter，而不是当普通消息丢进 inbox；
- 两次投递都真的进了父会话的模型上下文（父会话各跑一轮）。

断言面：事件时间线（两个 `user_message_accepted` 的正文）+ 子会话的 tool 结果（答案真的到了
那一问上）+ 落盘链配对不变量。
"""

from __future__ import annotations

import asyncio
import re
import time
from typing import Any

import pytest

from wing_probe import Probe, ToolCall, Turn
from wing_probe.watch import Event

CHILD_MODEL = "probe/wake-ask-child"
PARENT_MODEL = "probe/wake-ask-parent"

PARENT_ACK = "ack"
ANSWER = "red"

#: 英文信封里的定向地址（`tool_call_id=<hex>`）。
TOOL_CALL_ID = re.compile(r"tool_call_id=(\S+?)\)")

QUESTION = {
    "id": "color",
    "header": "Color",
    "question": "Which color?",
    "options": [{"label": "red"}, {"label": "blue"}],
}


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


def _envelope_text(event: Event) -> str:
    return event.data.get("content") or ""


@pytest.mark.probe_env(models=[CHILD_MODEL, PARENT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_ask_wake_then_answer_then_done(probe: Probe) -> None:
    """子问 → 父收 ask 信封；照信封里的 id 回答 → 子收口 → 父再收 done 信封。"""
    probe.register(
        CHILD_MODEL,
        Turn.of(tool_calls=[ToolCall("AskUserQuestion", {"questions": [QUESTION]})]),
        Turn.of(text="answered"),
    )
    probe.register(PARENT_MODEL, *[Turn.of(text=PARENT_ACK) for _ in range(8)])

    parent = await probe.session(model=PARENT_MODEL)
    child = await probe.session(model=CHILD_MODEL)

    created = await probe.driver_required.http.create_wake(
        kind="session",
        target_session_id=parent.session_id,
        source_session_id=child.session_id,
    )
    wake = created["wake"]

    # ── 子会话提问 ──
    await child.send("ask me something")
    ask_event = await parent.watch.expect(
        "user_message_accepted",
        where=lambda e: "is waiting for your answer" in _envelope_text(e),
        within=20,
    )
    ask_envelope = _envelope_text(ask_event)
    assert f"[wing] session {child.session_id} is waiting for your answer" in (
        ask_envelope
    ), ask_envelope
    assert "Question:\n- [Color] Which color?  (red / blue)" in ask_envelope, (
        ask_envelope
    )
    match = TOOL_CALL_ID.search(ask_envelope)
    assert match, ask_envelope
    tool_call_id = match.group(1)
    assert f"Answer: wing run -r {child.session_id} --tool-call-id {tool_call_id}" in (
        ask_envelope
    ), ask_envelope

    # ask 唤醒不消耗 done 配额：触发器仍在，配额没动
    active = (await probe.driver_required.http.list_wakes())["wakes"]
    assert [(w["id"], w["state"], w["fire_count"]) for w in active] == [
        (wake["id"], "active", 1)
    ], active

    # ── 父会话照着信封回答（定向回那一个 ask） ──
    await child.answer(tool_call_id, ANSWER)
    child_turn = await child.watch.expect("turn_result", within=20)
    assert child_turn.data["result"] == "answered", child_turn.data

    tool_result = child.watch.events("tool_call_result")
    answered = [
        event for event in tool_result if event.data.get("tool_call_id") == tool_call_id
    ]
    assert answered, [e.data for e in tool_result]
    assert answered[0].data["tool_success"] is True, answered[0].data

    # ── 子会话收口 → 父会话再收一次 done 信封 ──
    done_event = await parent.watch.expect(
        "user_message_accepted",
        where=lambda e: "finished" in _envelope_text(e),
        within=20,
    )
    done_envelope = _envelope_text(done_event)
    assert f"[wing] session {child.session_id} finished" in done_envelope
    assert "subtype=success" in done_envelope
    assert "Last message:\nanswered" in done_envelope

    finished = await _wait_wake(probe, wake["id"], state="done")
    assert finished["fire_count"] == 2, finished  # ask 一次 + done 一次

    # 两条唤醒消息都在父会话的落盘链上
    messages = probe.history(parent).messages()
    wake_messages = [
        message["content"]
        for message in messages
        if message["role"] == "user"
        and "[wing] session" in (message.get("content") or "")
    ]
    assert len(wake_messages) == 2, wake_messages
