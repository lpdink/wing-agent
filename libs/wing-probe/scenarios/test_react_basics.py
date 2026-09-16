"""ReAct 基础场景（tasks 6.2–6.4，spec probe-scenarios「Requirement: ReAct 基础场景」）。

覆盖的断言点：

- ``test_text_turn_event_order_and_persistence``：文本轮——事件序
  （``user_message_accepted → turn_started → text* → assistant_turn → turn_result``）、
  ``text`` 事件拼接 == 剧本文本、无 ``error``、活跃链上 user/assistant 消息记录 +
  transient 记录不落盘；
- ``test_bash_tool_roundtrip_and_pairing``：Bash 工具往返——``tool_call`` /
  ``tool_call_result`` 事件与真实输出、workspace 文件效果、**下一轮请求**里
  assistant(tool_calls) ↔ tool 结果配对与回灌、``turn_result`` 到达；
- ``test_ask_roundtrip_and_raw_answer_feedback``：ask 往返——``ask`` 事件字段
  （questions / tool_call_id）、经 WS 定向答复（携带 ``tool_call_id``）、裸答复
  回灌进下一轮请求、对话继续至 ``turn_result``、全程无 ``error``。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn

#: 场景私有 model 名（design D3：剧本按 model 名路由，场景之间不共享）。
TEXT_MODEL = "probe/react-text"
BASH_MODEL = "probe/react-bash"
ASK_MODEL = "probe/react-ask"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_text_turn_event_order_and_persistence(probe: Probe) -> None:
    """文本轮事件序与落盘（spec: 文本轮事件序与落盘）。

    WHEN 假 Provider 剧本返回单文本轮，场景发送一条消息
    THEN 断言通过：事件序为 ``user_message_accepted → turn_started → text* →
    assistant_turn → turn_result``；``text`` 事件拼接等于剧本文本；无 ``error``；
    活跃链上存在 user / assistant 消息记录且无 transient 记录。

    > 与 spec 文本的差异（如实记录）：spec 把 ``turn_started`` 写在
    > ``user_message_accepted`` 之前；实现（``react_loop.run_turn``）的注释与
    > 行为都是"``user_message_accepted`` 先于 ``turn_started``——事件流顺序即
    > 渲染顺序"。断言按实跑口径写，并在交付报告"发现的问题"里归类说明。
    """
    probe.register(
        TEXT_MODEL,
        # 分片（chunk=4）→ 多个 text 事件，拼接断言才有意义。
        Turn.of(text="hello world", chunk=4),
    )
    session = await probe.session(model=TEXT_MODEL)

    result = await session.chat("hi")

    assert result.data["subtype"] == "success", result.data
    assert result.data["is_error"] is False, result.data
    assert result.data["result"] == "hello world", result.data

    session.watch.assert_ordered(
        [
            "user_message_accepted",
            "turn_started",
            "text",
            "assistant_turn",
            "turn_result",
        ],
        since=0,
    )
    text_events = session.watch.events("text")
    assert len(text_events) > 1, [e.type for e in session.timeline.all()]
    assert "".join(event.data["content"] for event in text_events) == "hello world"
    session.watch.assert_never("error")

    view = session.history
    assert [message["role"] for message in view.messages()] == ["user", "assistant"]
    assert view.messages()[0]["content"] == "hi"
    assert view.messages()[1]["content"] == "hello world"
    # 红线：流式 delta 类瞬态记录绝不落盘（spec「transient 记录不落盘」）。
    view.assert_no_transient_records()
    view.assert_chain_invariants()
    view.assert_tool_pairing()

    context = probe.context(TEXT_MODEL, 0)
    assert context.system == "", "probe 网关的 system prompt 为空串（见实施期事实口径）"
    context.assert_prefix_like(["user: hi"])


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_bash_tool_roundtrip_and_pairing(probe: Probe) -> None:
    """Bash 工具往返与上下文配对（spec: Bash 工具往返与上下文配对）。

    WHEN 剧本发起一次 Bash 调用，工具在 workspace 真实执行
    THEN 断言通过：``tool_call`` / ``tool_call_result`` 事件到达且结果内容等于真实
    输出；文件系统出现命令效果；**下一轮 LLM 请求**中 assistant(tool_calls) 与
    tool 结果正确配对、输出内容回灌；最终 ``turn_result`` 到达。
    """
    probe.register(
        BASH_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall("Bash", {"command": "printf probe-out > out.txt && echo done"})
            ]
        ),
        Turn.of(text="all done"),
    )
    # yolo：Bash 免确认（本场景断言的是"真实执行 + 回灌"，不是安全审查）。
    session = await probe.session(model=BASH_MODEL, yolo=True)

    result = await session.chat("create the file")

    assert result.data["result"] == "all done", result.data

    # since=0：从时间线起点扫描（chat 已把游标推到 turn_result 之后）。
    session.watch.assert_ordered(
        ["tool_call", "tool_call_result", "turn_result"], since=0
    )
    tool_call = session.watch.events("tool_call")[0]
    assert tool_call.data["tool_name"] == "Bash", tool_call.data
    assert tool_call.data["tool_args"]["command"] == (
        "printf probe-out > out.txt && echo done"
    ), tool_call.data

    tool_result = session.watch.events("tool_call_result")[0]
    assert tool_result.data["tool_call_id"] == tool_call.data["tool_call_id"]
    assert tool_result.data["tool_success"] is True, tool_result.data
    assert "done" in tool_result.data["tool_result"], tool_result.data
    session.watch.assert_never("error")

    # 命令在 workspace 真实跑过（cwd = session workspace）。
    probe.files.assert_exists("out.txt")
    probe.files.assert_content("out.txt", equals="probe-out")

    # 第二轮请求的上下文：assistant(tool_calls) 与 tool 结果配对 + 输出回灌。
    assert probe.requests.count(BASH_MODEL) == 2, probe.requests.summary()
    follow_up = probe.context(BASH_MODEL, 1)
    follow_up.assert_tool_pairing()
    assert follow_up.role_sequence() == ["user", "assistant", "tool"], (
        follow_up.role_sequence()
    )
    assistant = follow_up.messages[1]
    assert assistant.call_ids == (tool_call.data["tool_call_id"],), assistant.summary()
    tool_message = follow_up.messages[2]
    assert tool_message.tool_call_id == tool_call.data["tool_call_id"]
    assert "done" in tool_message.content, tool_message.summary()

    # 落盘侧：tool 消息在活跃链上（配对不变量由 fixture teardown 再跑一遍）。
    view = session.history
    assert [message["role"] for message in view.messages()] == [
        "user",
        "assistant",
        "tool",
        "assistant",
    ]
    view.assert_tool_pairing()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_ask_roundtrip_and_raw_answer_feedback(probe: Probe) -> None:
    """ask 往返与裸答复回灌（spec: ask 往返与裸答复回灌）。

    WHEN 剧本调用 AskUserQuestion，场景经 WS 答复（携带 ``tool_call_id``）
    THEN 断言通过：``ask`` 事件字段（questions / tool_call_id）正确；答复后工具结果
    被回灌进下一轮请求；对话继续至 ``turn_result``，全程无 ``error``。
    """
    probe.register(
        ASK_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall(
                    "AskUserQuestion",
                    {
                        "questions": [
                            {
                                "id": "flavor",
                                "header": "Flavor",
                                "question": "Which flavor?",
                                "options": [
                                    {"label": "vanilla"},
                                    {"label": "mocha", "description": "with cocoa"},
                                ],
                            }
                        ]
                    },
                )
            ]
        ),
        Turn.of(text="thanks for answering"),
    )
    session = await probe.session(model=ASK_MODEL)

    await session.send("pick something")
    ask = await session.watch.expect("ask", within=15)

    assert ask.data["tool_call_id"], ask.data
    assert ask.data["questions"] == [
        {
            "id": "flavor",
            "header": "Flavor",
            "question": "Which flavor?",
            "multiSelect": False,
            "options": [
                {"label": "vanilla", "description": ""},
                {"label": "mocha", "description": "with cocoa"},
            ],
        }
    ], ask.data["questions"]

    await session.answer(ask, "mocha")
    result = await session.watch.expect("turn_result", within=15)

    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")
    session.watch.assert_ordered(["ask", "tool_call_result"], since=0)

    tool_result = session.watch.events("tool_call_result")[0]
    assert tool_result.data["tool_result"] == "mocha", tool_result.data

    assert probe.requests.count(ASK_MODEL) == 2, probe.requests.summary()
    follow_up = probe.context(ASK_MODEL, 1)
    follow_up.assert_tool_pairing()
    tool_message = follow_up.messages[-1]
    assert tool_message.role == "tool", tool_message.summary()
    assert tool_message.content == "mocha", tool_message.summary()
    assert tool_message.tool_call_id == ask.data["tool_call_id"]

    view = session.history
    assert [message["role"] for message in view.messages()] == [
        "user",
        "assistant",
        "tool",
        "assistant",
    ]
    view.assert_tool_pairing()
    # ask 是事实事件（persist=True），随链落盘——rewind 场景据此构造"事件祖先"。
    assert [event["type"] for event in view.events()] == [
        "user_message_accepted",
        "turn_started",
        "ask",
        "turn_result",
        "done",
    ], [(event["type"], event["uuid"]) for event in view.events()]
