"""落盘故障不把会话打成僵尸（#187）——真网关进程上的端到端验收。

单元测试（`libs/core/tests/test_worker_resilience.py`）直接注入 ENOSPC 测 worker
的收口与续期；本场景把同一条链拉到进程边界之外：真网关 + 假 Provider 下让
**落盘整体失败**（去掉 history.jsonl 的写权限 ⇒ 追加时 `open(..., "ab")` 抛
OSError，与磁盘打满的 ENOSPC 同类），断言用户看得见的三件事：

1. **error 收口**：那一轮以 `turn_result[error_during_execution]` 结束，且
   `error` / `done` 都到达客户端——报告动作自身落盘也失败了（它写的是同一本
   刚坏掉的 history.jsonl），但它必须仍被下发：前端靠它复位 working 态；
2. **状态可复用**：会话回到 idle（`/api/session/info`），不是"卡在中途"；
3. **零僵尸**：落盘恢复后投递的消息**被真的消费**并成功收口（消息不落虚空，
   会话自然继续工作）。

故障注入是文件权限位（真实 OSError，无假 Provider / 网关侧钩子）：去掉写的
能力即复刻"同一故障击穿两层兜底 handler"的现场。root 无视权限位，故在 root
下跳过（CI 与本地开发都是普通用户）。

打开日志失败时，丢弃事件仅包含失败批次的输入 ID。追加与回截均失败时，两个
订阅者收到写入结果不明和 ``session_history_refreshed``。同一网关逐出再水合仍只
看到已确认的前缀。解除故障后，续写前回截，Provider 上下文与落盘历史一致。
"""

from __future__ import annotations

import os

import pytest

from wing_probe import Driver, Probe, Turn

MODEL = "probe/worker-resilience"

#: 磁盘打满时用户看到的故障文本（权限位注入：现场是 ENOSPC）。
FAILURE_MARKER = "Permission denied"


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_persist_outage_does_not_zombie_the_session(probe: Probe) -> None:
    """落盘整体失败 → error 收口 + 会话 idle；恢复后投递仍被消费。"""
    if not hasattr(os, "geteuid") or os.geteuid() == 0:
        pytest.skip(
            "root / 无权限位的平台：本场景靠去掉 history.jsonl 的写权限注入落盘失败"
        )

    probe.register(MODEL, Turn.of(text="first"), Turn.of(text="recovered"))
    session = await probe.session(model=MODEL)

    result = await session.chat("one")
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "first", result.data

    history = probe.env.session_dir(session.session_id) / "history.jsonl"
    assert history.is_file(), history

    # ── 注入：落盘写口不可写（追加时抛 OSError） ──
    os.chmod(history, 0o400)
    try:
        cursor = session.watch.cursor
        failed_id = await session.send("two")
        dropped = await session.watch.expect("user_messages_dropped")
        assert dropped.data["origin_request_ids"] == [failed_id]
        session.watch.assert_never(
            "user_message_committed", where={"origin_request_ids": [failed_id]}
        )
        assert [m["content"] for m in (await session.get())["messages"]] == [
            "one",
            "first",
        ]
        event = await session.watch.expect(["turn_result", "error"], within=30)

        # ① error 收口：报告三连都到达客户端（前端据此复位 working 态）。
        assert event.type == "turn_result", [e.type for e in session.timeline.all()]
        assert event.data["subtype"] == "error_during_execution", event.data
        assert event.data["is_error"] is True, event.data
        assert FAILURE_MARKER in event.data["errors"][-1], event.data
        # 尾部两帧要**等**到（expect 命中 turn_result 即返回，error / done 还在
        # 后续读循环里）；`assert_ordered` 是同步扫描、不等待，直接扫会读到
        # "还没到"的时间线（无 uvloop 的合帧节奏下必红）。等齐再验相对顺序。
        await session.watch.expect("error", within=30)
        await session.watch.expect("done", within=30)
        session.watch.assert_ordered(["turn_result", "error", "done"], since=cursor)

        # ② 状态复位：不是一个卡住的会话——下一次投递可以干净地用上它。
        info = await session.info()
        assert info["status"] == "idle", info
    finally:
        os.chmod(history, 0o600)

    # ③ 零僵尸：恢复后投递的消息被真的消费并成功收口。
    result = await session.chat("three")
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "recovered", result.data
    assert [m["content"] for m in probe.history(session).messages()] == [
        "one",
        "first",
        "three",
        "recovered",
    ]


@pytest.mark.probe_env(models=[MODEL], hooks=["hooks/*.py"])
@pytest.mark.asyncio
async def test_indeterminate_append_refreshes_two_subscribers_and_recovers_in_process(
    probe: Probe,
) -> None:
    probe.register(MODEL, Turn.of(text="first"), Turn.of(text="recovered"))
    session = await probe.session(model=MODEL)
    await session.chat("one")
    history = session.session_dir / "history.jsonl"
    hook_dir = probe.env.root / "hooks"
    hook_dir.mkdir()
    control = probe.env.root / "fault-enabled"
    (hook_dir / "fault.py").write_text(
        f"""
import os
from pathlib import Path

history = Path({str(history)!r})
control = Path({str(control)!r})
fsync = os.fsync
ftruncate = os.ftruncate

def affects(fd):
    if not control.exists():
        return False
    target = history.stat()
    opened = os.fstat(fd)
    return (target.st_dev, target.st_ino) == (opened.st_dev, opened.st_ino)

def fail_sync(fd):
    if affects(fd):
        raise OSError("test history fsync failure")
    return fsync(fd)

def fail_truncate(fd, size):
    if affects(fd):
        raise OSError("test history ftruncate failure")
    return ftruncate(fd, size)

os.fsync = fail_sync
os.ftruncate = fail_truncate
""",
        encoding="utf-8",
    )
    assert (await session.driver.http.request("POST", "/api/system/reload"))["ok"]
    other = await Driver.connect(probe.env)
    try:
        watcher = await other.attach(session.session_id)
        subscribed = await watcher.watch.expect("sync_session")
        control.touch()
        failed_id = await session.send("uncertain input")
        for subscriber in (session, watcher):
            await subscriber.watch.expect(
                "user_message_queued", where={"origin_request_id": failed_id}
            )
            error = await subscriber.watch.expect("error")
            assert "write outcome unknown" in error.data["message"]
            await subscriber.watch.expect("done")
            reconciled = await subscriber.watch.expect("session_history_refreshed")
            assert reconciled.data["pending_user_messages"] == []
            assert reconciled.data["status"] == "idle"
            assert reconciled.data["messages"] == subscribed.data["messages"]
            subscriber.watch.assert_never(
                "user_messages_dropped", where={"origin_request_ids": [failed_id]}
            )
        uncertain = history.read_bytes()
        assert b"uncertain input" in uncertain
        assert len(probe.requests_for(MODEL)) == 1
        await other.http.unsubscribe(session.session_id, other.client_id)
    finally:
        await other.close()

    driver = session.driver
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    assert (await session.release())["released"] is True
    await driver.http.subscribe(session.session_id, driver.client_id)
    sync = await session.watch.expect("sync_session")
    assert sync.data["messages"] == subscribed.data["messages"]
    assert sync.data["pending_user_messages"] == []
    assert history.read_bytes() == uncertain
    control.unlink()
    assert (await session.chat("three")).data["result"] == "recovered"
    assert probe.context(MODEL, 1).texts("user") == ["one", "three"]
    assert b"uncertain input" not in history.read_bytes()
    assert [m["content"] for m in probe.history(session).messages()] == [
        "one",
        "first",
        "three",
        "recovered",
    ]
