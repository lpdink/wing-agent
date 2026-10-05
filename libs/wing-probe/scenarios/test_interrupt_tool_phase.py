"""工具执行期中断红线场景（spec「工具执行期中断」）。

被守的语义（`libs/core/wing/agent/tool_executor.py` + `react_loop.py`）：

- 工具执行中被 interrupt：`ToolExecutor.execute` **不让 CancelledError 逃逸**——它收拢
  每个调用的最终结果（已完成的用真实结果、被取消的用合成结果
  `Tool call interrupted by user.`），抛 `InterruptedToolResults`；
- ReActLoop 沿**正常路径**提交本轮消息（assistant + 逐条 tool 结果），然后再抛出原始
  CancelledError 让 worker 终止——所以落盘链上 `assistant(tool_calls)` 与 tool 消息
  **配对完整**，没有任何 call_id 悬空（红线 3 的直接形态）；
- runtime 在旧 worker 补提交**之后**才落 `interrupted` 事件：链上事件节点必须位于
  补提交的 tool 消息之后（顺序反了就是补提交丢了）；
- `ToolCallResultEvent`（合成结果、`success=False`）已广播——前端据此关掉工具卡片；
- Bash 的 interrupt hook 杀掉工具子进程：被打断的命令不得留下**迟到副作用**。

断言面：事件时间线（`session.watch`）+ 落盘链（`session.history`）+ 下一轮请求体
（`probe.context`）+ workspace 文件。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from wing_probe import Probe, Session, ToolCall, Turn
from wing_probe.history import message_semantics

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
TOOL_PHASE_MODEL = "probe/interrupt-tool-phase"
BATCH_MODEL = "probe/interrupt-tool-phase-batch"

#: 被打断的工具调用写入的统一结果（`tool_executor.INTERRUPTED_RESULT`）。
INTERRUPTED_RESULT = "Tool call interrupted by user."

#: 慢命令：`sleep` 的秒数就是打断窗口（进程被 interrupt hook 杀掉，场景不等待它）。
LATE_SECONDS = 2
LATE_FILE = "late.txt"
SLOW_COMMAND = f"sleep {LATE_SECONDS} && printf late > {LATE_FILE}"

#: 并行场景：慢者被打断、快者保留真实结果。
SLOW_CALL = "call_slow"
FAST_CALL = "call_fast"
FAST_FILE = "fast.txt"
FAST_CONTENT = "written-before-interrupt"

#: 打断后会话回到 idle 的轮询预算（镜像 `test_interrupt.py` 的口径）。
IDLE_DEADLINE = 10.0
POLL_INTERVAL = 0.05


async def _wait_idle(session: Session, *, timeout: float = IDLE_DEADLINE) -> str:
    """轮询到会话回到 idle（中断收口：补提交完成、新 worker 就位）。"""
    expires = time.monotonic() + timeout
    status = ""
    while time.monotonic() < expires:
        status = str((await session.info()).get("status"))
        if status == "idle":
            return status
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session did not return to idle within {timeout}s: {status!r}"
    )


async def _assert_never_appears(
    probe: Probe, path: str, *, until: float, note: str
) -> None:
    """等过迟到窗口：文件一旦出现即失败，窗口走完仍未出现即通过。

    ``until`` 是**相对 env 启动的单调时钟**（与事件 ``at`` 同基准）——
    "窗口"由被打断的工具自己定义（它会在 sleep 之后写文件），所以这里等的是
    真实世界的迟到副作用，而不是给同步留时间。
    """
    files = probe.files
    deadline = probe.env.started_at + until
    while True:
        if files.resolve(path).exists():
            raise AssertionError(f"{note}: {path!r} appeared after the interrupt")
        if time.monotonic() >= deadline:
            return
        await asyncio.sleep(POLL_INTERVAL)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_interrupt_during_tool_execution_synthesizes_result(
    probe: Probe,
) -> None:
    """工具执行期打断：合成结果 + 配对完整 + 事件在补提交之后（红线）。

    WHEN 剧本发起一次长跑的 ``Bash``（``sleep`` + 迟到写文件）、工具已在执行
    THEN HTTP interrupt 后：落盘链是 ``user / assistant(tool_calls) / tool(合成结果)``、
    配对完整；``interrupted`` 事件节点在补提交**之后**（parent 指向 tool 消息）；
    事件时间线 ``tool_call → tool_call_result(合成结果, success=False) → interrupted``；
    会话回 idle 且能继续下一轮；被杀的子进程不留迟到副作用。
    """
    probe.register(
        TOOL_PHASE_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": SLOW_COMMAND}, id=SLOW_CALL)]),
        Turn.of(text="after interrupt"),
    )
    session = await probe.session(model=TOOL_PHASE_MODEL, yolo=True)

    await session.send("run a slow command")
    started = await session.watch.expect("tool_call", within=15)
    assert started.data["tool_name"] == "Bash", started.data
    assert started.data["tool_call_id"] == SLOW_CALL, started.data

    response = await session.interrupt()
    assert response.get("ok") is True, response
    # runtime 在 interrupt 返回前落盘 InterruptedEvent——广播帧随后到达。
    await session.watch.expect("interrupted", within=15)
    assert await _wait_idle(session) == "idle"
    session.watch.assert_never("error")

    # ── 落盘：assistant(tool_calls) + 合成结果，配对完整 ──
    view = session.history
    messages = view.messages()
    assert [message["role"] for message in messages] == ["user", "assistant", "tool"], (
        view.describe()
    )
    assistant, tool = messages[1], messages[2]
    # 生成本身**逐字完整**：块数组（含 tool_use 的参数）一字不少、也不多出半截文本 /
    # 思考——被切断的是工具执行阶段，不是流式生成阶段（那是 test_interrupt.py 的形态：
    # 半截文本 + stop_reason=interrupted）。这里对账的是"补提交内容零损"这个真实失败面。
    semantics = message_semantics(assistant)
    assert semantics["content_blocks"] == [
        {
            "type": "tool_use",
            "id": SLOW_CALL,
            "name": "Bash",
            "input": {"command": SLOW_COMMAND},
            "input_error": None,
        }
    ], assistant
    assert semantics["content"] is None, assistant
    assert semantics["reasoning_content"] is None, assistant
    assert semantics["tool_calls"] == [
        {
            "id": SLOW_CALL,
            "name": "Bash",
            "arguments": {"command": SLOW_COMMAND},
            "arguments_error": None,
        }
    ], assistant
    assert tool["tool_call_id"] == SLOW_CALL, tool
    assert tool["content"] == INTERRUPTED_RESULT, tool
    view.assert_tool_pairing()
    view.assert_chain_invariants()
    view.assert_no_transient_records()

    # ── interrupted 事件在链上位于补提交之后 ──
    events = view.events()
    assert [event["type"] for event in events].count("interrupted") == 1, (
        view.describe()
    )
    interrupted = events[-1]
    assert interrupted["type"] == "interrupted", view.describe()
    interrupted_uuid = interrupted["uuid"]
    tool_uuid = tool["uuid"]
    tool_line = view.line_of(tool_uuid)
    interrupted_line = view.line_of(interrupted_uuid)
    # 行号是可定位的硬前提：某个 uuid 不在记录里（line_of → None）时给可读的红，
    # 而不是让 `<` 以 TypeError 收场。
    assert tool_line is not None, view.describe()
    assert interrupted_line is not None, view.describe()
    assert tool_line < interrupted_line, (view.describe(),)
    parent = view.parent_of(interrupted_uuid)
    assert parent is not None and parent["uuid"] == tool_uuid, (view.describe(),)

    # ── 广播：前端能关卡片（tool_call → 合成结果 → interrupted） ──
    session.watch.assert_ordered(
        ["tool_call", "tool_call_result", "interrupted"], since=0
    )
    results = session.watch.events(type="tool_call_result")
    assert len(results) == 1, results
    assert results[0].data["tool_call_id"] == SLOW_CALL, results[0].data
    assert results[0].data["tool_result"] == INTERRUPTED_RESULT, results[0].data
    assert results[0].data["tool_success"] is False, results[0].data

    # ── 继续下一轮：合成结果参与下一轮请求，配对在请求体里同样成立 ──
    result = await session.chat("continue")
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "after interrupt", result.data
    follow_up = probe.context(TOOL_PHASE_MODEL, 1)
    follow_up.assert_tool_pairing()
    replayed = [message for message in follow_up.messages if message.role == "tool"]
    assert len(replayed) == 1, follow_up.describe()
    assert replayed[0].content == INTERRUPTED_RESULT, follow_up.describe()
    assert replayed[0].tool_call_id == SLOW_CALL, follow_up.describe()

    # ── 被动副作用红线：子进程被 interrupt hook 杀掉，sleep 之后的写入不会发生 ──
    await _assert_never_appears(
        probe,
        LATE_FILE,
        until=started.at + LATE_SECONDS + 0.7,
        note="Bash child process survived the interrupt",
    )


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_interrupt_collects_each_parallel_call_individually(probe: Probe) -> None:
    """并行调用逐个收拢：快者留真实结果、慢者合成结果，且按调用序对齐（红线）。

    WHEN 一轮里并发发起两个调用（慢 ``Bash`` + 快 ``Write``），快者已完成（其
    ``tool_call_result`` 已广播）时 interrupt
    THEN 落盘链的 tool 消息与 ``assistant.tool_calls`` **逐个对齐**（id 顺序一致）：
    快者是真实结果、慢者是合成结果——`zip(pending_tool_calls, results)` 的对齐语义。
    """
    probe.register(
        BATCH_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall("Bash", {"command": f"sleep {LATE_SECONDS}"}, id=SLOW_CALL),
                ToolCall(
                    "Write", {"path": FAST_FILE, "content": FAST_CONTENT}, id=FAST_CALL
                ),
            ]
        ),
        Turn.of(text="after interrupt"),
    )
    session = await probe.session(model=BATCH_MODEL, yolo=True)

    await session.send("run both")
    fast = await session.watch.expect(
        "tool_call_result", where={"tool_call_id": FAST_CALL}, within=15
    )
    assert fast.data["tool_result"] != INTERRUPTED_RESULT, fast.data
    assert fast.data["tool_success"] is True, fast.data
    probe.files.assert_content(FAST_FILE, equals=FAST_CONTENT)

    response = await session.interrupt()
    assert response.get("ok") is True, response
    await session.watch.expect("interrupted", within=15)
    assert await _wait_idle(session) == "idle"

    view = session.history
    messages = view.messages()
    assert [message["role"] for message in messages] == [
        "user",
        "assistant",
        "tool",
        "tool",
    ], view.describe()
    assistant = messages[1]
    assert [call["id"] for call in assistant["tool_calls"]] == [SLOW_CALL, FAST_CALL], (
        assistant
    )
    slow_result, fast_result = messages[2], messages[3]
    assert slow_result["tool_call_id"] == SLOW_CALL, slow_result
    assert slow_result["content"] == INTERRUPTED_RESULT, slow_result
    assert fast_result["tool_call_id"] == FAST_CALL, fast_result
    assert fast_result["content"] == fast.data["tool_result"], (
        fast.data,
        fast_result,
    )
    assert fast_result["content"] != INTERRUPTED_RESULT, fast_result
    view.assert_tool_pairing()
    view.assert_no_transient_records()

    # 广播序：快者的真实结果先到，慢者的合成结果随打断收尾到达。
    result_events = session.watch.events(type="tool_call_result")
    assert [event.data["tool_call_id"] for event in result_events] == [
        FAST_CALL,
        SLOW_CALL,
    ], [(event.index, event.data) for event in result_events]
    assert result_events[-1].data["tool_result"] == INTERRUPTED_RESULT, result_events[
        -1
    ].data
    assert result_events[-1].data["tool_success"] is False, result_events[-1].data
    session.watch.assert_ordered(["tool_call_result", "interrupted"], since=0)

    result = await session.chat("continue")
    assert result.data["result"] == "after interrupt", result.data
    probe.context(BATCH_MODEL, 1).assert_tool_pairing()
