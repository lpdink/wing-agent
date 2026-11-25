"""max_turns 触顶场景（工具链路面 · 覆盖点 3）。

被守的语义（`agent/react_loop.py::run_turn`）：

- 每轮开始时检查 `ctx.num_turns >= self.max_turns`：触顶即发
  `turn_result(subtype="error_max_turns", is_error=True)` + `done`，**不做**第 N+1 次
  LLM 调用——上限是"不许再花 token"的硬闸门，不是软提示；
- 触顶发生在轮**边界**：此前每一轮的 `assistant(tool_calls)` 都已有配对 tool 消息
  （补提交口径），链上不留孤儿 assistant；
- 触顶只结算**当前** turn：会话仍是可用状态，下一条消息开启的新 turn 从 `num_turns=0` 起。

断言面：事件时间线（subtype / is_error / errors / num_turns）、剧本消费次数（没有第三次
调用）、落盘链（与广播的工具结果逐字一致 + 配对完整）、workspace 文件（两轮工具都真跑过）。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from wing_probe import Probe, Session, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
MAX_TURNS_MODEL = "probe/max-turns"

#: 压到 2：两个工具轮之后触顶（第三个剧本 Turn 留给"触顶之后仍可对话"）。
MAX_TURNS = 2

#: 两轮工具各写一个文件——"真的跑了两轮"的 workspace 证据。
FIRST_FILE = "max-turns-1.txt"
SECOND_FILE = "max-turns-2.txt"

#: 触顶后的收口轮询预算。
IDLE_DEADLINE = 10.0
POLL_INTERVAL = 0.05


def _write_turn(path: str, content: str) -> Turn:
    """一轮工具调用：`Write` 覆盖指定文件（无 yolo / 危险命令交互，纯粹的执行面）。"""
    return Turn.of(tool_calls=[ToolCall("Write", {"path": path, "content": content})])


async def _wait_idle(session: Session, *, timeout: float = IDLE_DEADLINE) -> str:
    """轮询到会话回到 idle（触顶后 worker 收口、新 worker 就位）。"""
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


@pytest.mark.probe_env(models=[MAX_TURNS_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_max_turns_reports_error_and_keeps_chain_paired(probe: Probe) -> None:
    """触顶：`error_max_turns` + 配对完整 + 没有第三次调用（红线）。

    WHEN `max_turns=2` 且剧本每轮都发工具调用
    THEN `turn_result` 是 `error_max_turns` / `is_error=True` / `num_turns=2` / `errors`
    非空；请求恰好 2 次（第三次调用不存在——剧本耗尽会 5xx，所以"多调用一次"是结构性
    失败）；链 = user / assistant+tool ×2，配对完整、无孤儿；两轮工具都真跑过；触顶后
    新消息开启的 turn 从 `num_turns=0` 起正常收口。
    """
    probe.register(
        MAX_TURNS_MODEL,
        _write_turn(FIRST_FILE, "first"),
        _write_turn(SECOND_FILE, "second"),
        Turn.of(text="after max turns"),
    )
    session = await probe.session(model=MAX_TURNS_MODEL, max_turns=MAX_TURNS)

    await session.send("do two rounds of work")
    result = await session.watch.expect("turn_result", within=30)
    assert result.data["subtype"] == "error_max_turns", result.data
    assert result.data["is_error"] is True, result.data
    assert result.data["num_turns"] == MAX_TURNS, result.data
    assert result.data["errors"], result.data
    # 锚产品真实文案（`react_loop` 的 "Reached max turns limit: <n>"）；承重的计数
    # 断言是上面的 num_turns（"含数字 2" 之类的弱锚不算数）。
    assert "Reached max turns limit" in result.data["errors"][0], result.data["errors"]
    assert await _wait_idle(session) == "idle"
    session.watch.assert_never("error")

    # ── 没有第三次调用：剧本三轮，这里只消费了前两轮 ──
    assert probe.requests.count(MAX_TURNS_MODEL) == MAX_TURNS, probe.requests.summary()

    # ── 链：两轮 tool 结果都提交了（与广播逐字一致），没有孤儿 assistant ──
    tool_results = [
        event.data["tool_result"]
        for event in session.watch.events(type="tool_call_result")
    ]
    assert len(tool_results) == MAX_TURNS, tool_results
    assert all(
        event.data["tool_success"]
        for event in session.watch.events(type="tool_call_result")
    )
    view = probe.history(session)
    assert [message["role"] for message in view.messages()] == [
        "user",
        "assistant",
        "tool",
        "assistant",
        "tool",
    ], view.describe()
    assert [
        message["content"] for message in view.messages() if message["role"] == "tool"
    ] == tool_results, view.describe()
    view.assert_tool_pairing()
    view.assert_chain_invariants()
    view.assert_no_transient_records()

    # ── workspace：两轮工具都真跑过 ──
    probe.files.assert_content(FIRST_FILE, equals="first")
    probe.files.assert_content(SECOND_FILE, equals="second")

    # ── 触顶只结算当前 turn：下一条消息开启的新 turn 从 0 起，正常收口 ──
    follow_up = await session.chat("carry on")
    assert follow_up.data["subtype"] == "success", follow_up.data
    assert follow_up.data["result"] == "after max turns", follow_up.data
    assert follow_up.data["num_turns"] == 1, follow_up.data
    assert probe.requests.count(MAX_TURNS_MODEL) == MAX_TURNS + 1, (
        probe.requests.summary()
    )
