"""危险命令确认场景（工具链路面 · 覆盖点 4）。

被守的语义（`tools/bash.py` + `tools/shell_safety.py`）：

- 非 yolo 会话里，未命中 `safe_command_patterns` 白名单的命令一律走**用户确认**
  （probe 的 `safe_command_patterns=[]` → 任何非空命令都算危险）；
- 确认走 `ctx.ask_feedback(AskEvent(...))`：事件带 `question`（含命令原文）、
  `choices=["y","n","yolo"]` 与**当前工具调用的 `tool_call_id`**（定向答复的地址）；
- 答 `n` → `ToolError("❌ Command rejected by user.")`：命令**不执行**（零副作用）；
  答 `y` → 正常执行并返回结果。回答串台（把 y/n 送错调用）是这条路径的典型回归。

断言面：事件时间线（Ask 载荷 + 归属）、工具结果（拒绝/执行）、workspace 文件、
落盘链（ask 是事实事件，落链 + 配对完整）。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Session, ToolCall, Turn
from wing_probe.watch import Event

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
CONFIRM_MODEL = "probe/dangerous-command"

#: 两条命令各写一个文件：拒绝 / 放行由 workspace 事实判定（不只看文案）。
REJECTED_FILE = "denied.txt"
APPROVED_FILE = "approved.txt"

#: Ask 的锚点（`bash.py::_handle_dangerous_command`；问句带 ⚠️ 前缀与围栏里的命令原文）。
ASK_ANCHOR = "Dangerous command detected"
REJECT_ANCHOR = "Command rejected by user."
CHOICES = ["y", "n", "yolo"]


def _dangerous_turn(command: str) -> Turn:
    return Turn.of(tool_calls=[ToolCall("Bash", {"command": command})])


@pytest.mark.probe_env(models=[CONFIRM_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_dangerous_command_ask_deny_then_allow(probe: Probe) -> None:
    """非 yolo：危险命令先问后跑——答 n 不执行、答 y 执行（红线）。

    WHEN `yolo=false` 的会话里同一剧本连发两条危险命令
    THEN 两条命令各触发一次 Ask（`choices` 齐全、`tool_call_id` 与各自的调用对齐）；
    答 `n` 的那条只留拒绝文案且**文件不存在**，答 `y` 的那条真的执行且文件内容逐字；
    ask 落链、assistant/tool 配对完整、没有任何 error 收场。
    """
    probe.register(
        CONFIRM_MODEL,
        _dangerous_turn(f"printf denied > {REJECTED_FILE}"),
        Turn.of(text="first command handled"),
        _dangerous_turn(f"printf approved > {APPROVED_FILE}"),
        Turn.of(text="second command handled"),
    )
    session = await probe.session(model=CONFIRM_MODEL)

    # ── 第一次：答 n ──
    await session.send("run the first dangerous command")
    first_ask = await session.watch.expect("ask", within=15)
    assert ASK_ANCHOR in first_ask.data["question"], first_ask.data
    assert f"printf denied > {REJECTED_FILE}" in first_ask.data["question"], (
        first_ask.data
    )
    assert first_ask.data["choices"] == CHOICES, first_ask.data
    first_call_id = first_ask.data["tool_call_id"]
    assert first_call_id, first_ask.data

    await session.answer(first_ask, "n")
    first_result = await session.watch.expect("turn_result", within=20)
    assert first_result.data["subtype"] == "success", first_result.data
    assert first_result.data["result"] == "first command handled", first_result.data

    rejected = _result_for(session, first_call_id)
    assert REJECT_ANCHOR in rejected.data["tool_result"], rejected.data
    assert rejected.data["tool_success"] is False, rejected.data
    probe.files.assert_missing(REJECTED_FILE)

    # ── 第二次：答 y（同一会话，回答必须回到**这一次**的调用上） ──
    await session.send("run the second dangerous command")
    second_ask = await session.watch.expect("ask", within=15)
    assert second_ask.data["choices"] == CHOICES, second_ask.data
    assert second_ask.data["tool_call_id"] not in ("", first_call_id), second_ask.data
    assert f"printf approved > {APPROVED_FILE}" in second_ask.data["question"], (
        second_ask.data
    )

    await session.answer(second_ask, "y")
    second_result = await session.watch.expect("turn_result", within=20)
    assert second_result.data["subtype"] == "success", second_result.data
    assert second_result.data["result"] == "second command handled", second_result.data

    approved = _result_for(session, second_ask.data["tool_call_id"])
    assert approved.data["tool_success"] is True, approved.data
    probe.files.assert_content(APPROVED_FILE, equals="approved")

    # ── 归属与落盘：两条 ask 各归各的调用，链上配对完整 ──
    asks = session.watch.events(type="ask")
    assert [event.data["tool_call_id"] for event in asks] == [
        first_call_id,
        second_ask.data["tool_call_id"],
    ], [event.data for event in asks]
    view = probe.history(session)
    view.assert_tool_pairing()
    view.assert_chain_invariants()
    assert [
        event["tool_call_id"] for event in view.events() if event["type"] == "ask"
    ] == [
        first_call_id,
        second_ask.data["tool_call_id"],
    ], view.describe()
    assert [message["role"] for message in view.messages()] == [
        "user",
        "assistant",
        "tool",
        "assistant",
        "user",
        "assistant",
        "tool",
        "assistant",
    ], view.describe()
    session.watch.assert_never("error")


def _result_for(session: Session, call_id: str) -> Event:
    """按 call_id 取那一次调用的工具结果事件（回答串台时报告里能直接看出送错了谁）。"""
    matches = [
        event
        for event in session.watch.events(type="tool_call_result")
        if event.data["tool_call_id"] == call_id
    ]
    assert len(matches) == 1, [
        event.data for event in session.watch.events(type="tool_call_result")
    ]
    return matches[0]
