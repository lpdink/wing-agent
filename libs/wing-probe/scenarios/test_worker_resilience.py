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

不变量逃生舱：注入落盘故障必然让**磁盘**链断开（故障期间该会话的写入整体
失败，内存链与磁盘分叉，恢复后新记录的 parent_uuid 指向一个没写进文件的
uuid）——这是 `TrackedList` 既有的"先改内存再落盘"语义在故障下的必然结果
（磁盘链修复不在 #187 范围）。本场景断言的是消费者存活与续期，故显式关闭
teardown 的内置不变量并给出理由。
"""

from __future__ import annotations

import os

import pytest

from wing_probe import Probe, Turn

MODEL = "probe/worker-resilience"

#: 磁盘打满时用户看到的故障文本（权限位注入：现场是 ENOSPC）。
FAILURE_MARKER = "Permission denied"


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_persist_outage_does_not_zombie_the_session(probe: Probe) -> None:
    """落盘整体失败 → error 收口 + 会话 idle；恢复后投递仍被消费。"""
    if os.geteuid() == 0:
        pytest.skip(
            "root 无视文件权限位：本场景靠去掉 history.jsonl 的写权限注入落盘失败"
        )

    probe.register(
        MODEL,
        # 第 2 个 Turn 可能被故障那轮吃掉（取决于故障落在哪一步）——第 2/3 个
        # Turn 同文，恢复轮的断言因此不依赖"故障轮是否发出过模型请求"。
        Turn.of(text="first"),
        Turn.of(text="recovered"),
        Turn.of(text="recovered"),
    )
    probe.without_invariants(
        "注入落盘故障必然让磁盘链断开（故障期间该会话写入整体失败，内存链与磁盘"
        "分叉）——本场景断言消费者存活与续期，磁盘链修复不在 #187 范围"
    )
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
        await session.send("two")
        event = await session.watch.expect(["turn_result", "error"], within=30)

        # ① error 收口：报告三连都到达客户端（前端据此复位 working 态）。
        assert event.type == "turn_result", [e.type for e in session.timeline.all()]
        assert event.data["subtype"] == "error_during_execution", event.data
        assert event.data["is_error"] is True, event.data
        assert FAILURE_MARKER in event.data["errors"][-1], event.data
        session.watch.assert_ordered(["error", "done"], since=cursor)

        # ② 状态复位：不是一个卡住的会话——下一次投递可以干净地用上它。
        info = await session.info()
        assert info["status"] == "idle", info
    finally:
        os.chmod(history, 0o600)

    # ③ 零僵尸：恢复后投递的消息被真的消费并成功收口。
    result = await session.chat("three")
    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "recovered", result.data
