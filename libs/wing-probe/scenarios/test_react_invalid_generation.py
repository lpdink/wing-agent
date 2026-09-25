"""无效轮次的自动重试（空响应 / 流被截断）——假 Provider 扮演「出错的网关」。

上游网关的两种讨厌行为（无法改网关，只能在 wing 侧容错）：

- **限流式空响应**：什么都不生成；
- **截断**：tool call 还没收敛（非法 JSON / 中途切断）就直接切断流。

两种都不该让 ReAct 静默中止：无效轮次交给有界重试，且必须可见（``notice``）。

覆盖断言点：

- ``test_empty_generation_retries_then_succeeds``：空响应 → 无效 → 重试成功；
  事件序 ``user_message_accepted → turn_started → notice → text →
  assistant_turn → turn_result``；notice 带 attempt / max_attempts；空轮不落链；
  重试请求上下文与首轮一致（无效轮次没有塞进任何消息）。
- ``test_truncated_tool_call_keeps_content_and_retries``：未收敛 tool call 被
  截断 → content 提交、tool call 不提交 → 重试（续跑）；补发的 tool call 真实
  执行；链配对与不变量成立；请求侧可见「content → 补发 tool call → 工具结果」
  的推进。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn

EMPTY_MODEL = "probe/react-invalid-empty"
TRUNCATED_MODEL = "probe/react-invalid-truncated"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_empty_generation_retries_then_succeeds(probe: Probe) -> None:
    """限流式空响应：无效 → 重试 → 成功（空轮不落链、notice 可见）。"""
    probe.register(
        EMPTY_MODEL,
        Turn.of(),  # ① 空响应（无 content、无 tool call、无 reasoning）
        Turn.of(text="recovered"),  # ② 重试成功
    )
    session = await probe.session(model=EMPTY_MODEL)

    result = await session.chat("go")

    assert result.data["subtype"] == "success", result.data
    assert result.data["is_error"] is False, result.data
    assert result.data["result"] == "recovered", result.data

    session.watch.assert_ordered(
        [
            "user_message_accepted",
            "turn_started",
            "notice",
            "text",
            "assistant_turn",
            "turn_result",
        ],
        since=0,
    )
    notices = session.watch.events("notice")
    assert len(notices) == 1, [e.type for e in session.timeline.all()]
    assert notices[0].data["attempt"] == 1, notices[0].data
    assert notices[0].data["max_attempts"] >= 2, notices[0].data
    session.watch.assert_never("error")

    # 空轮不落链：历史只有正常的 user/assistant。
    view = session.history
    assert [m["role"] for m in view.messages()] == ["user", "assistant"]
    assert view.messages()[1]["content"] == "recovered"
    view.assert_no_transient_records()
    view.assert_chain_invariants()
    view.assert_tool_pairing()

    # 两次请求：重试请求的上下文与首轮一致（无效轮次没有塞进任何消息）。
    assert probe.requests.count(EMPTY_MODEL) == 2, probe.requests.summary()
    first = probe.context(EMPTY_MODEL, 0)
    second = probe.context(EMPTY_MODEL, 1)
    assert first.role_sequence() == ["user"], first.describe()
    assert second.role_sequence() == ["user"], second.describe()
    second.assert_tail_from([{"role": "user", "content": "go"}])


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_truncated_tool_call_keeps_content_and_retries(probe: Probe) -> None:
    """未收敛 tool call 被截断：content 提交 + 重试续跑，补发的调用真实执行。"""
    probe.register(
        TRUNCATED_MODEL,
        # ① 有 content、tool call 起了头但流被切断（无 finish 帧 → 未收敛）。
        Turn.of(
            text="let me check",
            tool_calls=[ToolCall("Bash", {"command": "echo must-not-run"})],
            truncated=True,
        ),
        # ② 续跑：模型补发 tool call。
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "printf x > f.txt"})]),
        # ③ 收尾。
        Turn.of(text="done"),
    )
    session = await probe.session(model=TRUNCATED_MODEL, yolo=True)

    result = await session.chat("go")

    assert result.data["result"] == "done", result.data

    session.watch.assert_ordered(
        ["turn_started", "notice", "tool_call", "tool_call_result", "turn_result"],
        since=0,
    )
    notices = session.watch.events("notice")
    assert len(notices) == 1, [e.type for e in session.timeline.all()]
    assert notices[0].data["attempt"] == 1, notices[0].data
    session.watch.assert_never("error")

    # 被截断的调用从未执行（只有补发的一次真实 tool_call）；补发的调用落盘成功。
    tool_calls = session.watch.events("tool_call")
    assert len(tool_calls) == 1, [e.data for e in tool_calls]
    assert tool_calls[0].data["tool_args"]["command"] == "printf x > f.txt", tool_calls[
        0
    ].data
    probe.files.assert_content("f.txt", equals="x")

    # 链：content 提交、未收敛 tool call 不提交；补发后严格配对。两条连续
    # assistant 即「续跑」语义：第一条是截断轮的 content，第二条是补发的调用。
    view = session.history
    assert [m["role"] for m in view.messages()] == [
        "user",
        "assistant",
        "assistant",
        "tool",
        "assistant",
    ]
    assert view.messages()[1]["content"] == "let me check"
    assert view.messages()[1].get("tool_calls") is None
    assert view.messages()[4]["content"] == "done"
    view.assert_tool_pairing()
    view.assert_chain_invariants()
    view.assert_no_transient_records()

    # 请求侧推进：② 以已提交 content 结尾（续跑），③ 带 content + 补发调用 +
    # 工具结果。
    assert probe.requests.count(TRUNCATED_MODEL) == 3, probe.requests.summary()
    retry_request = probe.context(TRUNCATED_MODEL, 1)
    assert retry_request.role_sequence() == ["user", "assistant"], (
        retry_request.describe()
    )
    retry_request.assert_tail_from([{"role": "assistant", "content": "let me check"}])
    follow_up = probe.context(TRUNCATED_MODEL, 2)
    assert follow_up.role_sequence() == ["user", "assistant", "assistant", "tool"], (
        follow_up.describe()
    )
    follow_up.assert_tool_pairing()
