"""steer 场景（工具链路面 · 覆盖点 2）。

被守的语义（`agent/react_loop.py::_llm_turn` + `agent/inbox.py`）：

- 工具执行期间到达的用户消息**入队不丢**（`Inbox` 队列，不打断当前轮）；
- 工具结果收拢之后，同一轮内 `drain_for_steer()` 把队列里的用户消息取走——逐条发射
  `user_message_accepted`（前端据此把排队消息上移进聊天历史），并经
  `Inbox.format_steer_note` 注入**最后一条**工具结果的文本头部，随下一轮请求
  一起交给模型（消息"往上走"当且仅当它真的被发给了模型）。

事件顺序按**源码事实**：`tool_call_result`（工具收尾事件，`ToolExecutor` 发射）先于
`user_message_accepted`（steer 发射点在 `execute()` 之后）——任务书写的"工具结果之前"
与代码不符，这里按代码断言（见 design.md Assumption A2）。

断言面：事件时间线（顺序 + `origin_request_id`）、下一轮请求体（note 前缀 + 工具输出）、
落盘链（消息没丢）。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
STEER_MODEL = "probe/steer-queued"
PLAIN_MODEL = "probe/steer-absent"

#: 工具跑得够久，保证第二条消息确实落在"工具执行期间"（窗口 = 工具时长）。
SLOW_COMMAND = "sleep 3"

#: 工具执行期间发出的第二条消息（steer 的载荷）。
STEER_TEXT = "also consider the failing test in read_image"

#: steer note 的格式锚点（`Inbox.format_steer_note`）。
NOTE_PREFIX = "[User steer note: "


def _note_for(text: str) -> str:
    """`format_steer_note` 的逐字形状（单条消息）。"""
    return f"{NOTE_PREFIX}{text}]\n"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_queued_message_steers_next_request(probe: Probe) -> None:
    """工具执行期间的第二条消息：入队 → accepted → 注入下一轮请求（红线）。

    WHEN 第一条消息触发一个慢工具（``sleep 3``），工具执行期间再发第二条
    THEN 第二条**没有**开启新一轮（`user_message_accepted` 恰在其工具结果之后发出、
    `origin_request_id` 等于它自己的 request_id），其内容以 steer note 形式**逐字**
    进入下一轮请求的最后一条 tool 消息头部，并同样落盘——消息没丢。
    """
    probe.register(
        STEER_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": SLOW_COMMAND})]),
        Turn.of(text="acknowledged the steer"),
    )
    session = await probe.session(model=STEER_MODEL, yolo=True)

    await session.send("start a slow tool")
    tool_call = await session.watch.expect("tool_call", within=15)
    assert tool_call.data["tool_name"] == "Bash", tool_call.data

    steer_request_id = await session.send(STEER_TEXT)
    assert steer_request_id, "上行帧必须带 request_id（origin_request_id 的来源）"

    result = await session.watch.expect("turn_result", within=30)
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "acknowledged the steer", result.data
    session.watch.assert_never("error")

    # ── 顺序：工具结果 → steer accepted → 本轮收口 ──
    session.watch.assert_ordered(
        ["tool_call", "tool_call_result", "user_message_accepted", "turn_result"],
        since=0,
    )
    accepted = session.watch.events(type="user_message_accepted")
    assert [event.data["content"] for event in accepted] == [
        "start a slow tool",
        STEER_TEXT,
    ], [event.data for event in accepted]
    tool_result = session.watch.events(type="tool_call_result")[0]
    steered = accepted[-1]
    assert steered.index > tool_result.index, (tool_result.index, steered.index)
    assert steered.data["origin_request_id"] == steer_request_id, steered.data
    assert tool_result.data["tool_success"] is True, tool_result.data

    # ── 下一轮请求：note 逐字前缀 + 原工具输出（顺序与拼接都不许漂） ──
    context = probe.context(STEER_MODEL, 1)
    context.assert_tool_pairing()
    assert context.role_sequence() == ["user", "assistant", "tool"], context.describe()
    tool_message = [message for message in context.messages if message.role == "tool"][
        0
    ]
    assert (
        tool_message.content == _note_for(STEER_TEXT) + tool_result.data["tool_result"]
    ), context.describe()
    assert STEER_TEXT in tool_message.content, context.describe()

    # ── 落盘链与请求体一致：steer 之后的消息留在 tool 消息里（没丢） ──
    view = probe.history(session)
    persisted = [message for message in view.messages() if message["role"] == "tool"]
    assert len(persisted) == 1, view.describe()
    assert persisted[0]["content"] == tool_message.content, view.describe()
    view.assert_tool_pairing()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_no_note_without_queued_message(probe: Probe) -> None:
    """对照：队列为空时工具结果**逐字**不被改写（不注入空 note）。

    WHEN 工具执行期间没有第二条消息
    THEN 下一轮请求里的 tool 消息 == 广播的工具结果（无 note 前缀、无多余换行）——
    "steer 只在真有排队消息时动 tool 结果"是这条路径的另一半。
    """
    probe.register(
        PLAIN_MODEL,
        Turn.of(tool_calls=[ToolCall("Write", {"path": "plain.txt", "content": "ok"})]),
        Turn.of(text="done"),
    )
    session = await probe.session(model=PLAIN_MODEL)
    result = await session.chat("write the file")
    assert result.data["subtype"] == "success", result.data
    probe.files.assert_content("plain.txt", equals="ok")

    tool_result = session.watch.events(type="tool_call_result")[0]
    context = probe.context(PLAIN_MODEL, 1)
    tool_message = [message for message in context.messages if message.role == "tool"][
        0
    ]
    assert tool_message.content == tool_result.data["tool_result"], context.describe()
    assert NOTE_PREFIX not in tool_message.content, context.describe()

    # 唯一的 accepted 是 turn 入口的 drain-and-merge（在工具调用之前）——队列为空时
    # 不该出现第二个（steer）发射点。
    accepted = session.watch.events(type="user_message_accepted")
    assert [event.data["content"] for event in accepted] == ["write the file"], [
        event.data for event in accepted
    ]
    assert accepted[0].index < session.watch.events(type="tool_call")[0].index, (
        "turn 入口的 accepted 必须早于工具调用"
    )
