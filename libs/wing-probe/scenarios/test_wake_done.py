"""唤醒场景 1：子会话收口 → 父会话收到 done 信封。

被守的语义（`wing/wake/` + `/api/wake/*` + `runtime.post`）：

- `POST /api/wake/create`（kind=session，source=子、target=父）落下一条 `active` 触发器；
- 子会话跑完一轮（`TurnResultEvent`，任意 subtype）→ 父会话里出现一条**普通用户消息**，
  内容是 done 信封：`[wing] session <child> finished (subtype=success, turns=N, Xs)` +
  最后一条 assistant 正文 + Inspect / Continue 两行行动指引；
- 投递带 `request_id`（父会话把它当普通用户消息渲染并据此关联排队态）；
- done 送达后 done 配额归零 → 触发器 `state=done`（整条终结，不再留 ask 空转）。

父会话真跑一轮（它是真会话，唤醒消息进 inbox 后照常驱动模型）——断言锚在事件 + 落盘链 +
`/api/wake/list` 三个面。
"""

from __future__ import annotations

import asyncio
import re
import time
from typing import Any

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由；父子各一条，剧本互不干扰）。
CHILD_MODEL = "probe/wake-done-child"
PARENT_MODEL = "probe/wake-done-parent"

#: 父会话脚本：每收到一条唤醒消息就回一句（储备足够多轮，多余请求不会发生）。
PARENT_ACK = "ack"


async def _wait_message_count(
    probe: Probe, session_id: str, expected: int, *, within: float = 20.0
) -> None:
    """等到落盘链里有 ``expected`` 条消息（投递已进上下文且已收口）。"""
    deadline = time.monotonic() + within
    count = 0
    while time.monotonic() < deadline:
        count = len(probe.history(session_id).messages())
        if count >= expected:
            return
        await asyncio.sleep(0.05)
    raise AssertionError(f"{session_id} has {count} messages, expected {expected}")


async def _session_status(probe: Probe, session_id: str) -> str | None:
    """``/api/session/list`` 里的运行时状态（``inactive`` = 已被逐出内存）。"""
    payload = await probe.driver_required.http.request("GET", "/api/session/list")
    for entry in payload.get("sessions", []):
        if entry.get("id") == session_id:
            return entry.get("status")
    return None


async def _wait_status(probe: Probe, session_id: str, expected: str) -> None:
    deadline = time.monotonic() + 20.0
    observed: str | None = None
    while time.monotonic() < deadline:
        observed = await _session_status(probe, session_id)
        if observed == expected:
            return
        await asyncio.sleep(0.05)
    raise AssertionError(
        f"session {session_id} stayed {observed!r}, expected {expected!r}"
    )


async def _wake_row(probe: Probe, wake_id: int) -> dict[str, Any]:
    payload = await probe.driver_required.http.list_wakes(all=True)
    for wake in payload["wakes"]:
        if wake["id"] == wake_id:
            return wake
    raise AssertionError(f"wake #{wake_id} disappeared: {payload['wakes']}")


async def _wait_history(
    probe: Probe, session_id: str, needle: str, *, within: float = 20.0
) -> str:
    """等到会话落盘链里出现一条含 ``needle`` 的 user 消息，返回它的正文。"""
    deadline = time.monotonic() + within
    messages: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        messages = probe.history(session_id).messages()
        for message in messages:
            content = message.get("content") or ""
            if message["role"] == "user" and needle in content:
                return content
        await asyncio.sleep(0.05)
    raise AssertionError(f"{needle!r} never landed in {session_id}: {messages}")


async def _wait_wake(
    probe: Probe, wake_id: int, *, state: str, within: float = 20.0
) -> dict[str, Any]:
    """轮询 ``/api/wake/list?all=true`` 直到该触发器进入期望状态。"""
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


@pytest.mark.probe_env(models=[CHILD_MODEL, PARENT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_done_wake_reaches_the_parent(probe: Probe) -> None:
    """父订阅子会话的收口：子跑完 → 父收到 done 信封，且触发器就此终结。"""
    probe.register(CHILD_MODEL, Turn.of(text="patched the parser"))
    probe.register(PARENT_MODEL, *[Turn.of(text=PARENT_ACK) for _ in range(8)])

    parent = await probe.session(model=PARENT_MODEL)
    child = await probe.session(model=CHILD_MODEL)

    created = await probe.driver_required.http.create_wake(
        kind="session",
        target_session_id=parent.session_id,
        source_session_id=child.session_id,
    )
    wake = created["wake"]
    assert wake["state"] == "active", wake
    assert wake["on_events"] == "done,ask", wake

    result = await child.chat("do the thing")
    assert result.data["subtype"] == "success", result.data

    # 父会话：唤醒消息是一条普通用户消息（被消费进上下文时发射 user_message_accepted）
    event = await parent.watch.expect(
        "user_message_accepted",
        where=lambda e: "[wing] session" in (e.data.get("content") or ""),
        within=20,
    )
    envelope = event.data["content"]
    assert f"[wing] session {child.session_id} finished" in envelope, envelope
    assert "subtype=success" in envelope and "turns=" in envelope, envelope
    assert "Last message:\npatched the parser" in envelope, envelope
    assert f"Inspect:   wing tail {child.session_id}" in envelope, envelope
    assert f'Continue:  wing run -r {child.session_id} -p "..."' in envelope, envelope

    # 父会话真跑了一轮（消息进了模型上下文，不是一条只挂在界面上的旁注）
    parent_turn = await parent.watch.expect("turn_result", within=20)
    assert parent_turn.data["result"] == PARENT_ACK, parent_turn.data

    # 落盘链：父会话历史里有一条以信封开头的 user 消息
    messages = probe.history(parent).messages()
    assert any(
        message["role"] == "user" and "[wing] session" in (message.get("content") or "")
        for message in messages
    ), messages

    finished = await _wait_wake(probe, wake["id"], state="done")
    assert finished["fire_count"] == 1, finished
    assert finished["last_fire_at"], finished

    # done 送达即终结：默认列表（只看 active）里它已经消失
    assert (await probe.driver_required.http.list_wakes())["wakes"] == []

    # 信封里的 id 就是子会话（防止把 source / target 写反）
    assert re.search(r"session \S+ finished", envelope)


#: 「target 已被逐出」场景：父会话不订阅（逐出要求没人订阅），断言只读落盘链。
EVICTED_PARENT_MODEL = "probe/wake-done-evicted-parent"
EVICTED_CHILD_MODEL = "probe/wake-done-evicted-child"


@pytest.mark.probe_env(models=[EVICTED_PARENT_MODEL, EVICTED_CHILD_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_wake_rehydrates_an_evicted_target(probe: Probe) -> None:
    """target 被逐出（release）后收口：投递自带水合，父会话照常收到信封。

    `docs/dev/wake.md` §1 承诺「target 已逐出 → 投递自带水合（`ensure_loaded`）」。
    逐出要求会话不在忙碌、没人订阅，所以父会话以 `subscribe=False` 建（本场景不看它的
    事件流，只读它落盘的历史——那才是「它确实被叫醒了」的持久证据）。
    """
    probe.register(EVICTED_CHILD_MODEL, Turn.of(text="child done"))
    probe.register(EVICTED_PARENT_MODEL, *[Turn.of(text=PARENT_ACK) for _ in range(4)])

    parent = await probe.session(model=EVICTED_PARENT_MODEL, subscribe=False)
    child = await probe.session(model=EVICTED_CHILD_MODEL)
    http = probe.driver_required.http

    # 先让它真的用过一轮（否则它还没有落盘，`release` 之后连"已被逐出"都无从观察）：
    # 用 HTTP 发一条（不订阅也能驱动），等到助手回复进了历史。
    await http.request(
        "POST",
        "/api/session/send",
        body={"session_id": parent.session_id, "content": "hello"},
    )
    await _wait_message_count(probe, parent.session_id, 2)

    wake = (
        await http.create_wake(
            kind="session",
            target_session_id=parent.session_id,
            source_session_id=child.session_id,
        )
    )["wake"]

    released = await parent.release()
    assert released == {"ok": True, "released": True, "detail": "released"}, released
    # 逐出对外的唯一可见痕迹：会话从 idle 变 inactive（= 不在网关内存里）；
    # 再 release 一次幂等回报 not loaded，作为"确实不在内存"的第二重证据。
    await _wait_status(probe, parent.session_id, "inactive")
    assert await parent.release() == {
        "ok": True,
        "released": False,
        "detail": "not loaded",
    }

    result = await child.chat("finish the job")
    assert result.data["subtype"] == "success", result.data

    message = await _wait_history(probe, parent.session_id, "[wing] session", within=25)
    assert f"[wing] session {child.session_id} finished" in message, message
    finished = await _wait_wake(probe, wake["id"], state="done")
    assert finished["fire_count"] == 1, finished
    # 水合之后的父会话真的跑了一轮（状态回到 idle，而不是停留在 inactive）
    await _wait_status(probe, parent.session_id, "idle")


INTERRUPT_CHILD_MODEL = "probe/wake-done-interrupt-child"
INTERRUPT_PARENT_MODEL = "probe/wake-done-interrupt-parent"

#: 慢速流式：打断窗口够宽（分片 4 字符 + 每片 50ms）。
INTERRUPT_TEXT = (
    "stream-until-interrupted and-keep-going-long-enough-for-the-interrupt-"
    "to-land-mid-flight so-that-the-turn-never-produces-a-turn-result"
)


@pytest.mark.probe_env(models=[INTERRUPT_CHILD_MODEL, INTERRUPT_PARENT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_interrupt_does_not_wake_the_parent(probe: Probe) -> None:
    """source 被 interrupt 不触发唤醒（中断不发射 `turn_result`，既有口径）。"""
    probe.register(
        INTERRUPT_CHILD_MODEL,
        Turn.of(text=INTERRUPT_TEXT, chunk=4, delay=0.05),
        Turn.of(text="after interrupt"),
    )
    probe.register(
        INTERRUPT_PARENT_MODEL, *[Turn.of(text=PARENT_ACK) for _ in range(4)]
    )

    parent = await probe.session(model=INTERRUPT_PARENT_MODEL)
    child = await probe.session(model=INTERRUPT_CHILD_MODEL)

    wake = (
        await probe.driver_required.http.create_wake(
            kind="session",
            target_session_id=parent.session_id,
            source_session_id=child.session_id,
        )
    )["wake"]

    await child.send("start a long stream")
    await child.watch.expect("text", within=15)  # 流已经在跑
    assert (await child.interrupt()).get("ok") is True
    await child.watch.expect("interrupted", within=15)

    # 给误投递一个窗口，然后确认父会话什么也没收到、触发器还在武装
    await asyncio.sleep(1.0)
    wake_messages = [
        message
        for message in probe.history(parent.session_id).messages()
        if message["role"] == "user"
        and "[wing] session" in (message.get("content") or "")
    ]
    assert wake_messages == [], wake_messages
    row = await _wake_row(probe, wake["id"])
    assert (row["state"], row["fire_count"]) == ("active", 0), row
