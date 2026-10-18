"""落盘记录的规范形：assistant 只存 content_blocks、null 不落盘、存量照旧加载。

链模型的硬约束之一是"禁存冗余"（见 issue #214）：``content`` /
``reasoning_content`` / ``tool_calls`` 是 ``content_blocks`` 的派生物（``Message``
的访问器实时计算），落盘只留 blocks——链上每一条消息因此只有**一份**存储，
``history.jsonl`` 的体积按派生物占比下降。

本场景锁两条红线：

1. **新记录**：真跑一轮（thinking + text + tool call）后，assistant 记录不带
   那三个派生键、不带值为 null 的键；tool 记录的 ``content`` 是存储值，保留。
   派生字段从盘上读回来必须**逐字**等于模型吐出的内容（blocks 无损）。
2. **存量兼容**：把盘上的记录改回"本变更之前"的形态（补回派生扁平字段 = 旧
   代码会写的样子），逐出后 resume——消息内容 / 推理 / 工具调用与改动前一致
   （读取路径 blocks 优先）；随后再发一轮，新记录是规范形、旧行**一字未改**
   （append-only，不迁移存量）。
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path
from typing import Any

import pytest

from wing_probe import Probe, ToolCall, Turn
from wing_probe.history import HistoryView, derive_flat_fields

#: 场景私有 model 名（剧本按 model 名路由，场景之间不共享）。
SHAPE_MODEL = "probe/record-shape"

#: 逐出观察预算（release 是显式动作，不需要 TTL 倒计时）。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.2

#: 剧本：thinking + text + 一次 Bash（覆盖三类块），随后收尾。
THINKING = "think about the file first"
TEXT = "writing a marker"
COMMAND = "printf shape > marker.txt"
TOOL_TEXT = "marker written"


def _records(probe: Probe, session_id: str) -> list[dict[str, Any]]:
    """原始记录（不经派生补齐——形状断言只看这个）。"""
    return HistoryView(probe.env.session_dir(session_id)).records


def _raw_lines(probe: Probe, session_id: str) -> list[str]:
    path: Path = probe.env.session_dir(session_id) / "history.jsonl"
    return [line for line in path.read_text(encoding="utf-8").splitlines() if line]


async def _evict(probe: Probe, session_id: str) -> None:
    """退订 → release → 等 ``inactive``（磁盘成为唯一事实来源）。"""
    driver = probe.driver_required
    await driver.http.unsubscribe(session_id, driver.client_id)
    payload = await driver.http.request(
        "POST", "/api/session/release", body={"session_id": session_id}
    )
    assert payload == {"ok": True, "released": True, "detail": "released"}, payload
    deadline = time.monotonic() + POLL_DEADLINE
    observed: str | None = None
    while time.monotonic() < deadline:
        listing = await driver.http.request("GET", "/api/session/list")
        for entry in listing.get("sessions", []):
            if entry.get("id") == session_id:
                observed = entry.get("status")
        if observed == "inactive":
            return
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(f"session {session_id} stayed in status {observed!r}")


async def _resubscribe(probe: Probe, session_id: str) -> None:
    """补订阅（``resume`` 复用已挂载句柄且不重复订阅，退订过的必须显式补）。"""
    driver = probe.driver_required
    await driver.http.subscribe(session_id, driver.client_id)


def _rewrite_as_legacy_shape(probe: Probe, session_id: str) -> list[str]:
    """把盘上的记录改回本变更之前的形态，返回改写后的行（用于 append-only 对账）。

    "旧形态"= 当时的 ``_to_record`` 输出：assistant 记录除 ``content_blocks`` 外
    还带派生的 ``content`` / ``reasoning_content`` / ``tool_calls``。字段值取
    :func:`derive_flat_fields`——与旧代码的导出逐字一致（存量实测 0 差异）。
    """
    path: Path = probe.env.session_dir(session_id) / "history.jsonl"
    lines: list[str] = []
    for record in _records(probe, session_id):
        legacy = dict(record)
        if legacy.get("role") == "assistant" and "content_blocks" in legacy:
            for key, value in derive_flat_fields(legacy).items():
                if value is not None:
                    legacy[key] = value
        lines.append(json.dumps(legacy, ensure_ascii=False))
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return lines


@pytest.mark.probe_env(models=[SHAPE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_records_store_content_blocks_only(probe: Probe) -> None:
    """新记录只存 ``content_blocks``：派生字段与 null 键都不落盘（无损往返）。

    WHEN 剧本产出一轮 thinking + text + Bash 调用
    THEN assistant 记录没有 content / reasoning_content / tool_calls，也没有
    值为 null 的键；同一份内容从记录读回来逐字等于剧本。
    """
    probe.register(
        SHAPE_MODEL,
        Turn.of(
            thinking=THINKING,
            text=TEXT,
            tool_calls=[ToolCall("Bash", {"command": COMMAND})],
        ),
        Turn.of(text=TOOL_TEXT),
    )
    session = await probe.session(model=SHAPE_MODEL, yolo=True)

    result = await session.chat("write the marker")
    assert result.data["result"] == TOOL_TEXT, result.data

    records = _records(probe, session.session_id)
    assistants = [r for r in records if r.get("role") == "assistant"]
    assert assistants, records
    for record in assistants:
        for key in ("content", "reasoning_content", "tool_calls"):
            assert key not in record, record
        assert record["content_blocks"], record

    # null 不写：链拓扑字段与可选字段缺席（不是 None）
    for record in records:
        nulls = [key for key, value in record.items() if value is None]
        assert not nulls, (nulls, record)

    # tool 记录：content 是**存储值**（工具结果），不属于派生物
    tools = [r for r in records if r.get("role") == "tool"]
    assert [r["content"] for r in tools] and all(
        isinstance(r["content"], str) for r in tools
    ), tools

    # 派生字段从盘上读回（blocks 是唯一存储，必须无损）
    view = session.history
    messages = view.messages()
    assert [m["role"] for m in messages] == ["user", "assistant", "tool", "assistant"]
    first = messages[1]
    assert first["reasoning_content"] == THINKING, first
    assert first["content"] == TEXT, first
    assert first["tool_calls"][0]["arguments"] == {"command": COMMAND}, first
    assert messages[3]["content"] == TOOL_TEXT, messages[3]

    # 网关内存态与盘上事实一致（工具参数原样：自由载荷不进任何剥离）
    context = probe.context(SHAPE_MODEL, 1)
    assert context.messages[1].content == TEXT, context.describe()

    view.assert_record_shape()
    view.assert_chain_invariants()
    view.assert_tool_pairing()


@pytest.mark.probe_env(models=[SHAPE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_records_still_load_and_stay_untouched(probe: Probe) -> None:
    """存量兼容：旧形态记录照旧加载；新一轮只追加规范形，旧行一字未改。

    WHEN 盘上记录被改回旧形态（带派生扁平字段）后 resume，再发一轮
    THEN resume 的消息内容 / 推理 / 工具调用与旧形态一致（blocks 优先）；
    追加的新记录是规范形；旧行逐字节未变（不迁移存量、不重写）。

    场景**故意注入**了旧形态记录（模拟本变更之前的存量会话）。落盘规范形的
    内置不变量只对**实现写出**的记录负责，因此断言完成后把注入行还原成规范形
    ——否则夹具会把测试注入误报成产品缺陷（没有别的逃生舱，也不该有）。
    """
    probe.register(
        SHAPE_MODEL,
        Turn.of(
            thinking="legacy thinking",
            tool_calls=[ToolCall("Bash", {"command": "printf legacy > marker.txt"})],
        ),
        Turn.of(text="legacy reply"),
        Turn.of(text="after resume"),
    )
    session = await probe.session(model=SHAPE_MODEL, yolo=True)
    first = await session.chat("first question")
    assert first.data["result"] == "legacy reply", first.data
    session_id = session.session_id
    before = await session.get()
    await _evict(probe, session_id)

    legacy_lines = _rewrite_as_legacy_shape(probe, session_id)
    # 旧形态确实带上了派生字段（否则本场景没有验证到兼容路径）
    assert any('"reasoning_content"' in line for line in legacy_lines), legacy_lines
    assert any('"tool_calls"' in line for line in legacy_lines), legacy_lines

    try:
        resumed = await probe.resume(session_id)
        await _resubscribe(probe, session_id)
        after = await resumed.get()
        assert [m["role"] for m in after["messages"]] == [
            m["role"] for m in before["messages"]
        ], after
        assistant = after["messages"][1]
        assert assistant["content"] == "", assistant  # 纯工具调用轮（投影为 ""）
        assert assistant["reasoning_content"] == "legacy thinking", assistant
        assert assistant["tool_calls"][0]["arguments"] == {
            "command": "printf legacy > marker.txt"
        }, assistant
        assert after["messages"][3]["content"] == "legacy reply", after

        result = await resumed.chat("second question")
        assert result.data["result"] == "after resume", result.data

        # 旧行原样保留（append-only），新行是规范形
        lines = _raw_lines(probe, session_id)
        assert lines[: len(legacy_lines)] == legacy_lines, (
            "legacy records were rewritten"
        )
        tail = [json.loads(line) for line in lines[len(legacy_lines) :]]
        assert tail, lines
        new_assistants = [r for r in tail if r.get("role") == "assistant"]
        assert new_assistants, tail
        for record in new_assistants:
            for key in ("content", "reasoning_content", "tool_calls"):
                assert key not in record, record

        resumed.history.assert_chain_invariants()
    finally:
        # 注入行还原成规范形（见 docstring）——放在 finally 里，断言中途失败
        # 时不让夹具把注入误报成产品缺陷。
        _restore_canonical_shape(probe, session_id, injected=len(legacy_lines))
    probe.history(session_id).assert_record_shape()


def _restore_canonical_shape(probe: Probe, session_id: str, *, injected: int) -> None:
    """把场景注入的旧形态行还原为规范形（只动注入行，其余原样）。"""
    path: Path = probe.env.session_dir(session_id) / "history.jsonl"
    lines = [line for line in path.read_text(encoding="utf-8").splitlines() if line]
    for index in range(injected):
        record = json.loads(lines[index])
        if record.get("role") == "assistant" and "content_blocks" in record:
            for key in ("content", "reasoning_content", "tool_calls"):
                record.pop(key, None)
        lines[index] = json.dumps(record, ensure_ascii=False)
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
