"""中断场景（interrupt）：真实网关下的打断往返与可取证日志。

覆盖点：

- ``test_interrupt_mid_stream_closes_turn_and_keeps_chain_valid``：
  慢速流式生成中经 HTTP ``POST /api/session/interrupt`` 打断——中断 RPC
  正常返回；半截文本作为 partial assistant 消息提交（``stop_reason="interrupted"``）；
  ``interrupted`` 事件落盘并广播；会话回到 idle 并可继续下一轮对话；
  链不变量（teardown 自动）保持。
- ``test_interrupt_leaves_forensic_log_lines``：
  同一条打断路径在网关自身日志里留下分段日志（端点到达 / cancel 快照 /
  锁获取 / worker 退休 / reset）——死锁类现场（2026-09-28）无需 lldb 取证。
  断言面：``$WING_HOME/core/logs/wing_*.log``（产品写出的日志文件）。
"""

from __future__ import annotations

import asyncio
import time
from pathlib import Path

import pytest

from wing_probe import Probe, Session, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
STREAM_MODEL = "probe/interrupt-stream"
LOG_MODEL = "probe/interrupt-log"

#: 慢速流式文本：分片 4 字符 + 每片 50ms → 约 3s 的流。长度即打断窗口的余量
#: （打断到达即切断，流越长场景耗时不变——只在打断本身延误时才多等）。
STREAM_TEXT = (
    "interrupt-me-while-i-stream the-answer-is-long-enough-to-be-cut-mid-flight "
    "and-the-partial-commit-must-survive the-window-between-first-chunk-and-"
    "interrupt-must-be-wide-enough-for-a-slow-scheduler to-keep-this-scenario-"
    "deterministic under CI load"
)


def _stream_script() -> tuple[Turn, Turn]:
    """第一轮慢速流式（被打断），第二轮用于验证打断后仍能继续对话。"""
    return (
        Turn.of(text=STREAM_TEXT, chunk=4, delay=0.05),
        Turn.of(text="after interrupt"),
    )


async def _start_and_interrupt(probe: Probe, model: str) -> Session:
    """建会话 → 发起慢速流式轮 → 等到流已开始 → HTTP 打断。"""
    probe.register(model, *_stream_script())
    session = await probe.session(model=model)

    await session.send("start a long stream")
    # 等到确实有内容在流（打断窗口正中；首帧无延迟）。
    await session.watch.expect("text", within=15)

    response = await session.interrupt()
    assert response.get("ok") is True, response
    # runtime 在 interrupt 返回前落盘 InterruptedEvent——广播帧随后到达。
    await session.watch.expect("interrupted", within=15)
    return session


async def _wait_idle(session: Session, *, timeout: float = 10.0) -> str:
    """轮询到会话回到 idle（中断收口：预提交完成、新 worker 就位）。"""
    deadline = time.monotonic() + timeout
    status = ""
    while time.monotonic() < deadline:
        status = str((await session.info()).get("status"))
        if status == "idle":
            return status
        await asyncio.sleep(0.05)
    raise AssertionError(
        f"session did not return to idle within {timeout}s: {status!r}"
    )


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_interrupt_mid_stream_closes_turn_and_keeps_chain_valid(
    probe: Probe,
) -> None:
    """流式打断：RPC 返回、partial 提交、事件落盘、会话可继续。"""
    session = await _start_and_interrupt(probe, STREAM_MODEL)

    assert await _wait_idle(session) == "idle"
    session.watch.assert_never("error")

    # ── 落盘：user + 半截 assistant（stop_reason=interrupted）+ interrupted 事件 ──
    view = session.history
    messages = view.messages()
    assert [message["role"] for message in messages] == ["user", "assistant"], (
        view.describe()
    )
    partial = messages[-1]
    assert partial.get("stop_reason") == "interrupted", partial
    content = str(partial.get("content") or "")
    assert content, partial  # 至少一段已提交（打断发生在流已开始之后）
    assert STREAM_TEXT.startswith(content), (content, STREAM_TEXT)
    assert "interrupted" in {event["type"] for event in view.events()}, view.describe()
    view.assert_chain_invariants()
    view.assert_tool_pairing()
    view.assert_no_transient_records()

    # ── 打断后继续：新 worker 正常收口第二轮 ──
    result = await session.chat("continue")
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "after interrupt", result.data

    # 第二轮请求的上下文：已提交的 partial assistant 参与组装（角色链完整）。
    follow_up = probe.context(STREAM_MODEL, 1)
    assert follow_up.role_sequence() == ["user", "assistant", "user"], (
        follow_up.role_sequence()
    )


def _wing_log_dir(probe: Probe) -> Path:
    """网关自身日志目录（``$WING_HOME/core/logs``；probe 的 WING_HOME 在 tmp 内）。"""
    return probe.env.wing_home / "core" / "logs"


def _read_wing_log(probe: Probe) -> str:
    log_dir = _wing_log_dir(probe)
    files = sorted(log_dir.glob("wing_*.log"))
    assert files, f"no wing_*.log under {log_dir}"
    return "\n".join(
        path.read_text(encoding="utf-8", errors="replace") for path in files
    )


async def _wait_for_log_lines(
    probe: Probe, *needles: str, timeout: float = 10.0
) -> str:
    """轮询到日志里出现全部关键行（文件 handler 逐条 flush，正常是即时可见）。"""
    deadline = time.monotonic() + timeout
    text = ""
    while time.monotonic() < deadline:
        text = _read_wing_log(probe)
        if all(needle in text for needle in needles):
            return text
        await asyncio.sleep(0.05)
    missing = [needle for needle in needles if needle not in text]
    raise AssertionError(
        f"gateway log missing {missing}; last 2000 chars:\n{text[-2000:]}"
    )


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_interrupt_leaves_forensic_log_lines(probe: Probe) -> None:
    """打断路径在网关日志里留下可 grep 的分段现场（下次无需 lldb）。"""
    await _start_and_interrupt(probe, LOG_MODEL)

    text = await _wait_for_log_lines(
        probe,
        "interrupt request: session_id=",
        "interrupt start [",
        "cancel snapshot [",
        "fut_waiter=",
        "interrupt lock acquired [",
        "interrupt old worker retired [",
        "Agent interrupted and reset [",
        "interrupt done: session_id=",
    )
    assert "outcome=cancelled" in text
    # cancel 快照一行给出判读三件套：cancelling 簿记（快照先于 cancel，必为 0）、
    # fut_waiter 类型（吞没路径判别器）与 worker 暂停点栈。
    snapshot = next(line for line in text.splitlines() if "cancel snapshot [" in line)
    assert "done=False cancelling=0" in snapshot, snapshot
    assert "fut_waiter=Future(pending)" in snapshot, snapshot
    assert "stack=[" in snapshot, snapshot
