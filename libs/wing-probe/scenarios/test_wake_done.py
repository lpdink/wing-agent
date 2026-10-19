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
