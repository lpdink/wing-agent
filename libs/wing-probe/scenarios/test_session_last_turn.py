"""收口事实（``last_turn_end`` / ``last_turn_subtype``）——「已完成」收件箱的服务端底座。

被守的语义（任务书 §数据模型 / §写点 / §probe 场景）：

- **收口即落盘**：回合以 ``turn_result`` 收尾时，会话 metadata 记下结束时刻与
  该轮 subtype（``success`` / ``error_during_execution`` / ``error_max_turns``，
  逐字透传）；``/api/session/list`` 行与 ``/api/session/info`` 同值，盘上
  ``metadata.json`` 是同一份（探针独立读盘对账，不依赖网关自述）；
- **逐出后仍在**（核心断言）：release 之后行变成 ``inactive``，两字段照旧
  ——列表读的是磁盘 metadata，不是内存态。收件箱因此跨逐出 / 跨重启成立；
- **中断不是交付**：中断沿 CancelledError 收场、不发 ``turn_result``——先核实
  事件序列（首轮即中断的会话整条时间线里没有 ``turn_result``），再断言字段
  保持上一轮值 / 仍为 null；
- **旧记录读出 null**：手写的 metadata.json（引入前的形态）在列表里两字段为
  null，永不伪装成已完成。

时间口径：两字段与 ``last_interaction`` 同为本地 naive ISO；两轮之间用短
sleep 保证可分辨（与 ``test_session_list_order`` 同口径）。
"""

from __future__ import annotations

import asyncio
import json
import time
from datetime import datetime

import pytest

from wing_probe import Probe, Session, Turn
from wing_probe.driver.session import Driver

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
TURN_MODEL = "probe/last-turn"
INTERRUPT_MODEL = "probe/last-turn-interrupt"

#: 慢速流式文本：分片 4 字符 + 每片 50ms → 约 4s 的流，给中断留足窗口
#: （打断到达即切断，流越长场景耗时不变）。
STREAM_TEXT = (
    "interrupt-me-while-i-stream this-text-exists-only-to-give-the-interrupt-"
    "a-wide-enough-window even-under-a-loaded-ci-scheduler so-the-scenario-"
    "stays-deterministic"
)

#: 两次 chat 之间的最小间隔（时间戳严格可分辨；µs 分辨率 + 顺序 HTTP 往返）。
SETTLE = 0.05

#: 状态轮询预算（本地 HTTP，实测亚秒级；放宽只为 CI 抖动）。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.05


def _driver(probe: Probe) -> Driver:
    return probe.driver_required


async def _rows(probe: Probe) -> list[dict]:
    payload = await _driver(probe).http.request("GET", "/api/session/list")
    rows = payload.get("sessions")
    assert isinstance(rows, list), payload
    return rows


async def _entry(probe: Probe, session_id: str) -> dict:
    rows = await _rows(probe)
    for row in rows:
        if row.get("id") == session_id:
            return row
    raise AssertionError(f"session {session_id} not in list: {rows}")


async def _wait_status(probe: Probe, session_id: str, expected: str) -> None:
    """轮询到该会话的运行时状态变成 ``expected``（超时即失败，带最后观察值）。"""
    observed: str | None = None
    deadline = time.monotonic() + POLL_DEADLINE
    while time.monotonic() < deadline:
        observed = (await _entry(probe, session_id)).get("status")
        if observed == expected:
            return
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session {session_id} stayed in status {observed!r} for "
        f"{POLL_DEADLINE:.0f}s, expected {expected!r}"
    )


def _turn_fields(row: dict) -> tuple[object, object]:
    return row.get("last_turn_end"), row.get("last_turn_subtype")


async def _wait_idle(session: Session, *, timeout: float = POLL_DEADLINE) -> None:
    """轮询到会话回到 idle（中断收口：补提交完成、新 worker 就位）。"""
    status = ""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        status = str((await session.info()).get("status"))
        if status == "idle":
            return
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session {session.session_id} stayed in status {status!r} for "
        f"{timeout:.0f}s, expected 'idle'"
    )


async def _start_and_interrupt_stream(session: Session) -> None:
    """发起一条慢速流式轮、等它真的开始流、HTTP 打断并等收口回 idle。"""
    await session.send("start a long stream")
    await session.watch.expect("text", within=15)
    response = await session.interrupt()
    assert response.get("ok") is True, response
    await session.watch.expect("interrupted", within=15)
    await _wait_idle(session)


@pytest.mark.probe_env(models=[TURN_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_last_turn_lands_on_list_and_info_and_survives_release(
    probe: Probe,
) -> None:
    """收口两字段：list / info / metadata.json 同值；逐出后行仍带（冷读磁盘）。"""
    probe.register(
        TURN_MODEL,
        Turn.of(text="first delivery"),
        Turn.of(text="second delivery"),
    )
    session = await probe.session(model=TURN_MODEL, yolo=True)

    result = await session.chat("round one")
    assert result.data["subtype"] == "success", result.data

    row = await _entry(probe, session.session_id)
    first_end, first_subtype = _turn_fields(row)
    assert isinstance(first_end, str) and first_end, row
    assert first_subtype == "success", row
    datetime.fromisoformat(first_end)  # 本地 naive ISO（与 last_interaction 同口径）

    info = await session.info()
    assert _turn_fields(info) == (first_end, "success"), info

    # 盘上是同一份事实（收口即落盘；列表只是 metadata 的投影）。
    recorded = probe.history(session).metadata()
    assert recorded is not None
    assert _turn_fields(recorded) == (first_end, "success"), recorded

    await asyncio.sleep(SETTLE)
    result = await session.chat("round two")
    assert result.data["subtype"] == "success", result.data

    row = await _entry(probe, session.session_id)
    second_end, second_subtype = _turn_fields(row)
    assert isinstance(second_end, str) and second_subtype == "success", row
    assert datetime.fromisoformat(second_end) > datetime.fromisoformat(first_end), (
        first_end,
        second_end,
    )

    # release（先退订——订阅中的会话被钉住）→ 行转 inactive，两字段照旧。
    driver = _driver(probe)
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    released = await driver.http.release_session(session.session_id)
    assert released.get("released") is True, released
    await _wait_status(probe, session.session_id, "inactive")

    row = await _entry(probe, session.session_id)
    assert row.get("status") == "inactive", row
    assert _turn_fields(row) == (second_end, "success"), row


@pytest.mark.probe_env(models=[INTERRUPT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_interrupted_turns_never_record_a_turn_end(probe: Probe) -> None:
    """中断不发 ``turn_result``：字段保持上一轮值；首轮即中断则两字段仍 null。"""
    probe.register(
        INTERRUPT_MODEL,
        Turn.of(text="delivered"),  # 会话 ① 第一轮（成功收口）
        Turn.of(text=STREAM_TEXT, chunk=4, delay=0.05),  # ① 第二轮（被打断）
        Turn.of(text=STREAM_TEXT, chunk=4, delay=0.05),  # 会话 ② 首轮（被打断）
    )

    # ① 已交付过一轮：中断不覆盖收口事实。
    delivered = await probe.session(model=INTERRUPT_MODEL)
    result = await delivered.chat("round one")
    assert result.data["subtype"] == "success", result.data
    row = await _entry(probe, delivered.session_id)
    end_before, subtype_before = _turn_fields(row)
    assert isinstance(end_before, str) and subtype_before == "success", row

    await _start_and_interrupt_stream(delivered)

    # 核实（不是假定）：这条时间线里只有一个 turn_result——第一轮那个。
    assert [event.type for event in delivered.watch.events("turn_result")] == [
        "turn_result"
    ], [(event.index, event.type) for event in delivered.watch.events()]
    row = await _entry(probe, delivered.session_id)
    assert _turn_fields(row) == (end_before, "success"), row

    # ② 首轮即中断：整条时间线零 turn_result，两字段仍是 null。
    fresh = await probe.session(model=INTERRUPT_MODEL)
    await _start_and_interrupt_stream(fresh)
    assert fresh.watch.events("turn_result") == [], [
        (event.index, event.type) for event in fresh.watch.events()
    ]
    row = await _entry(probe, fresh.session_id)
    assert _turn_fields(row) == (None, None), row


@pytest.mark.timeout(60)
@pytest.mark.asyncio
async def test_legacy_metadata_without_fields_reads_null(probe: Probe) -> None:
    """引入前的 metadata.json（无字段）在列表里读出 null。"""
    session_id = "legacy-done-field"
    session_dir = probe.env.session_dir(session_id)
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / "metadata.json").write_text(
        json.dumps(
            {"session_name": "legacy", "last_interaction": "2025-01-01T00:00:00"}
        ),
        encoding="utf-8",
    )
    (session_dir / "history.jsonl").write_text(
        json.dumps({"role": "user", "content": "old", "uuid": "u-legacy"}) + "\n",
        encoding="utf-8",
    )

    row = await _entry(probe, session_id)
    assert _turn_fields(row) == (None, None), row
