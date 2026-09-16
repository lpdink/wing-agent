"""history.jsonl 解析与不变量对账（tasks 5.5）。

纯单测：手工构造 ``history.jsonl`` / ``metadata.json`` 写入 ``tmp_path``，
不起网关。覆盖：

- 解析：活跃链自末条记录回溯 / 事件与消息混排 / 损坏行跳过 / 缺失文件；
- 链拓扑不变量：uuid 重复、parent 断裂、记录无 uuid、未知记录形态、
  事件缺 type、tip 与末条记录不一致、成环（每条坏数据都断言"失败 + 定位"）；
- tool 配对不变量：缺配对、孤儿 tool、半截参数、arguments_error、call_id 重复；
- 瞬态记录不落盘：text / reasoning / tool_call_stream（含链外记录）+ 合法事实
  事件不误伤。
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest

from wing_probe.history import (
    DELTA_EVENT_TYPES,
    HistoryAssertionError,
    HistoryView,
)

TS = "2026-01-01T00:00:00"


# ── 构造工具（贴合产品落盘形状：assistant 的 content_blocks 是真相源） ──


def _prune(value: Any) -> Any:
    """剔除 None 字段（镜像 ``TrackedList._to_record`` 的落盘口径）。"""
    if isinstance(value, dict):
        return {key: _prune(item) for key, item in value.items() if item is not None}
    if isinstance(value, list):
        return [_prune(item) for item in value]
    return value


def message(
    role: str,
    *,
    uuid: str,
    parent: str | None = None,
    content: str | None = None,
    reasoning: str | None = None,
    tool_calls: list[dict[str, Any]] | None = None,
    tool_call_id: str | None = None,
    raw_arguments: Any = None,
    unzip: str | None = None,
) -> dict[str, Any]:
    """一条 Message 记录（assistant 同时带 content_blocks 与派生扁平字段）。"""
    record: dict[str, Any] = {
        "role": role,
        "uuid": uuid,
        "parent_uuid": parent,
        "ts": TS,
    }
    if unzip is not None:
        record["unzip_last_uuid"] = unzip
    if role == "assistant":
        blocks: list[dict[str, Any]] = []
        if reasoning:
            blocks.append(
                {"type": "thinking", "thinking": reasoning, "redacted": False}
            )
        if content:
            blocks.append({"type": "text", "text": content})
        for call in tool_calls or []:
            blocks.append(
                {
                    "type": "tool_use",
                    "id": call["id"],
                    "name": call["name"],
                    "input": call.get("arguments", {}),
                }
            )
        if blocks:
            record["content_blocks"] = blocks
        if content:
            record["content"] = content
        if reasoning:
            record["reasoning_content"] = reasoning
        if tool_calls:
            record["tool_calls"] = tool_calls
        if raw_arguments is not None:
            record["tool_calls"] = raw_arguments
            record.pop("content_blocks", None)
    else:
        if content is not None:
            record["content"] = content
        if tool_call_id:
            record["tool_call_id"] = tool_call_id
    return _prune(record)


def event(uuid: str, *, parent: str | None, type_: str, **extra: Any) -> dict[str, Any]:
    """一条事件记录（``role="event"`` + type）。"""
    return _prune(
        {
            "role": "event",
            "type": type_,
            "uuid": uuid,
            "parent_uuid": parent,
            "request_id": "req",
            "ts": TS,
            **extra,
        }
    )


def call(call_id: str, name: str, arguments: Any) -> dict[str, Any]:
    return {
        "id": call_id,
        "name": name,
        "arguments": arguments,
        "arguments_error": None,
    }


def write_history(session_dir: Path, records: Sequence[dict[str, Any] | str]) -> Path:
    """写 ``history.jsonl``（元素为 str 时原样写行，用于构造损坏行）。"""
    session_dir.mkdir(parents=True, exist_ok=True)
    path = session_dir / "history.jsonl"
    lines = [
        record if isinstance(record, str) else json.dumps(record, ensure_ascii=False)
        for record in records
    ]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def linear_history() -> list[dict[str, Any]]:
    """u1 → a1 → u2（含一条事件）：一张最小但完整的历史。"""
    return [
        message("user", uuid="u1", content="你好"),
        message("assistant", uuid="a1", parent="u1", content="在的"),
        event("e1", parent="a1", type_="diff_content", path="x.py"),
        message("user", uuid="u2", parent="e1", content="看看 x.py"),
    ]


def view_of(tmp_path: Path, records: Sequence[dict[str, Any] | str]) -> HistoryView:
    session_dir = tmp_path / "sess-1"
    write_history(session_dir, records)
    return HistoryView(session_dir)


# ── 解析 ────────────────────────────────────────────────────────


def test_missing_file_is_empty_view(tmp_path: Path) -> None:
    view = HistoryView(tmp_path / "nope")
    assert view.exists is False
    assert view.records == []
    assert view.by_uuid == {}
    assert view.active_chain() == []
    assert view.tip_uuid is None
    view.assert_chain_invariants()


def test_active_chain_walks_back_from_last_record(tmp_path: Path) -> None:
    view = view_of(tmp_path, linear_history())
    assert [record["uuid"] for record in view.active_chain()] == [
        "u1",
        "a1",
        "e1",
        "u2",
    ]
    assert view.tip_uuid == view.last_record_uuid == "u2"
    assert [record["uuid"] for record in view.messages()] == ["u1", "a1", "u2"]
    assert [record["uuid"] for record in view.events()] == ["e1"]
    parent = view.parent_of("e1")
    assert parent is not None and parent["uuid"] == "a1"
    assert view.find("nope") is None
    assert view.line_of("u2") == 4


def test_detached_nodes_stay_in_records_but_leave_the_chain(tmp_path: Path) -> None:
    """rewind 形态：新 tip 是复制行，旧节点仍在 records 但不在活跃链。"""
    records = linear_history()
    records.append(message("user", uuid="u3", parent="e1", content="重发的消息"))
    view = view_of(tmp_path, records)
    assert [record["uuid"] for record in view.active_chain()] == [
        "u1",
        "a1",
        "e1",
        "u3",
    ]
    assert view.find("u2") is not None
    assert "u2" not in [record["uuid"] for record in view.active_chain()]


def test_corrupt_lines_are_skipped(tmp_path: Path) -> None:
    records: list[dict[str, Any] | str] = [
        json.dumps(message("user", uuid="u1", content="hi")),
        "{not json at all",
        json.dumps(message("assistant", uuid="a1", parent="u1", content="hello")),
        "42",
        "",
    ]
    view = view_of(tmp_path, records)
    assert [record["uuid"] for record in view.records] == ["u1", "a1"]
    assert view.corrupt_lines == [2, 4]
    assert [record["uuid"] for record in view.active_chain()] == ["u1", "a1"]
    view.assert_chain_invariants()
    assert "corrupt lines skipped: [2, 4]" in view.describe()


def test_full_chain_follows_compact_boundary(tmp_path: Path) -> None:
    """完整链跨压缩边界：压缩节点 + 沿 unzip_last_uuid 回到被压缩区间。"""
    records = [
        message("user", uuid="u1", content="one"),
        message("assistant", uuid="a1", parent="u1", content="two"),
        message("assistant", uuid="c1", content="[Compact] 摘要", unzip="a1"),
        message("user", uuid="u2", parent="c1", content="three"),
    ]
    view = view_of(tmp_path, records)
    assert [record["uuid"] for record in view.active_chain()] == ["c1", "u2"]
    assert [record["uuid"] for record in view.full_chain()] == ["u1", "a1", "c1", "u2"]
    assert [record["uuid"] for record in view.full_chain("a1")] == ["u1", "a1"]


def test_reload_returns_new_snapshot(tmp_path: Path) -> None:
    session_dir = tmp_path / "sess-1"
    write_history(session_dir, [message("user", uuid="u1", content="hi")])
    view = HistoryView(session_dir)
    write_history(
        session_dir,
        [
            message("user", uuid="u1", content="hi"),
            message("assistant", uuid="a1", parent="u1", content="hello"),
        ],
    )
    assert [record["uuid"] for record in view.active_chain()] == ["u1"]
    assert [record["uuid"] for record in view.reload().active_chain()] == ["u1", "a1"]


# ── 链拓扑不变量 ────────────────────────────────────────────────


def test_chain_invariants_pass_on_healthy_history(tmp_path: Path) -> None:
    view = view_of(tmp_path, linear_history())
    view.assert_chain_invariants()
    assert "session sess-1" in view.describe()


def test_chain_invariants_detect_duplicated_uuid(tmp_path: Path) -> None:
    records = linear_history()
    records.append(message("assistant", uuid="a1", parent="u2", content="再答一次"))
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_chain_invariants()
    text = str(excinfo.value)
    assert "duplicated uuid 'a1'" in text
    assert "line 5" in text
    assert "first seen at line 2" in text


def test_chain_invariants_detect_dangling_parent(tmp_path: Path) -> None:
    records = [
        message("user", uuid="u1", content="hi"),
        message("assistant", uuid="a1", parent="ghost", content="hello"),
    ]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_chain_invariants()
    text = str(excinfo.value)
    assert "parent_uuid=ghost is not in the record set" in text
    assert "line 2" in text
    assert "chain[0]" in text  # 回溯在 a1 处中断（活跃链不可达根）


def test_chain_invariants_detect_missing_uuid(tmp_path: Path) -> None:
    records = linear_history()
    records[-1].pop("uuid")
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_chain_invariants()
    assert "missing uuid" in str(excinfo.value)
    assert "line 4" in str(excinfo.value)


def test_chain_invariants_detect_unknown_record_kind(tmp_path: Path) -> None:
    records = linear_history() + [{"role": "tool_result", "uuid": "x1", "content": "?"}]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_chain_invariants()
    text = str(excinfo.value)
    assert "unknown record kind role='tool_result'" in text
    assert "line 5" in text


def test_chain_invariants_detect_event_without_type(tmp_path: Path) -> None:
    records = linear_history() + [{"role": "event", "uuid": "e2", "parent_uuid": "u2"}]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_chain_invariants()
    assert "event record without a usable 'type'" in str(excinfo.value)


def test_chain_invariants_detect_tip_record_mismatch(tmp_path: Path) -> None:
    """末条记录是祖先：写盘顺序异常 → tip 判定与末条记录不一致。"""
    records = [
        message("assistant", uuid="a1", parent="u1", content="hello"),
        message("user", uuid="u1", content="hi"),
    ]
    view = view_of(tmp_path, records)
    assert view.tip_uuid == "a1"
    assert view.last_record_uuid == "u1"
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_chain_invariants()
    assert "!= last record uuid='u1'" in str(excinfo.value)


def test_chain_invariants_detect_cycle(tmp_path: Path) -> None:
    records = [
        message("user", uuid="u1", parent="a1", content="hi"),
        message("assistant", uuid="a1", parent="u1", content="hello"),
    ]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_chain_invariants()
    assert "cycle detected" in str(excinfo.value)


# ── tool 配对不变量 ─────────────────────────────────────────────


def paired_history(*, second_call_result: bool = True) -> list[dict[str, Any]]:
    records = [
        message("user", uuid="u1", content="跑两个命令"),
        message(
            "assistant",
            uuid="a1",
            parent="u1",
            content=None,
            tool_calls=[
                call("call_1", "Bash", {"command": "ls"}),
                call("call_2", "Bash", {"command": "pwd"}),
            ],
        ),
        message("tool", uuid="t1", parent="a1", content="a.txt", tool_call_id="call_1"),
    ]
    if second_call_result:
        records.append(
            message(
                "tool", uuid="t2", parent="t1", content="/tmp", tool_call_id="call_2"
            )
        )
    records.append(
        message("assistant", uuid="a2", parent=records[-1]["uuid"], content="done")
    )
    return records


def test_tool_pairing_passes_on_healthy_history(tmp_path: Path) -> None:
    view = view_of(tmp_path, paired_history())
    view.assert_tool_pairing()


def test_tool_pairing_detects_missing_pair(tmp_path: Path) -> None:
    view = view_of(tmp_path, paired_history(second_call_result=False))
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_tool_pairing()
    text = str(excinfo.value)
    assert "missing paired tool message with tool_call_id='call_2'" in text
    assert "chain[1]" in text


def test_tool_pairing_detects_orphan_tool_message(tmp_path: Path) -> None:
    records = [
        message("user", uuid="u1", content="跑"),
        message(
            "assistant",
            uuid="a1",
            parent="u1",
            tool_calls=[call("call_1", "Bash", {"command": "ls"})],
        ),
        message("tool", uuid="t1", parent="a1", content="a.txt", tool_call_id="call_1"),
        message("tool", uuid="tx", parent="t1", content="?", tool_call_id="call_9"),
    ]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_tool_pairing()
    text = str(excinfo.value)
    assert "orphan" in text
    assert "call_9" in text
    assert "uuid=tx" in text


def test_tool_pairing_detects_truncated_arguments(tmp_path: Path) -> None:
    records = [
        message("user", uuid="u1", content="跑"),
        message(
            "assistant",
            uuid="a1",
            parent="u1",
            raw_arguments=[
                {
                    "id": "call_1",
                    "name": "Bash",
                    # 半截参数：流式中断时被切断的 JSON 文本（旧形态落盘）
                    "arguments": '{"command": "ls',
                }
            ],
        ),
        message("tool", uuid="t1", parent="a1", content="?", tool_call_id="call_1"),
    ]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_tool_pairing()
    text = str(excinfo.value)
    assert "arguments are not valid JSON" in text
    assert "call_1" in text


def test_tool_pairing_detects_arguments_error(tmp_path: Path) -> None:
    records = [
        message("user", uuid="u1", content="跑"),
        message(
            "assistant",
            uuid="a1",
            parent="u1",
            raw_arguments=[
                {
                    "id": "call_1",
                    "name": "Bash",
                    "arguments": {},
                    "arguments_error": 'Expecting \',\' delimiter: {"command": "ls',
                }
            ],
        ),
        message(
            "tool", uuid="t1", parent="a1", content="bad json", tool_call_id="call_1"
        ),
    ]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_tool_pairing()
    assert "arguments_error recorded" in str(excinfo.value)
    # 有意构造的坏参数场景：显式放开后不再报错
    view.assert_tool_pairing(allow_arguments_error=True)


def test_tool_pairing_detects_duplicate_call_id(tmp_path: Path) -> None:
    records = paired_history()
    records[1]["tool_calls"].append(call("call_1", "Bash", {"command": "ls -l"}))
    records[1]["content_blocks"].append(
        {
            "type": "tool_use",
            "id": "call_1",
            "name": "Bash",
            "input": {"command": "ls -l"},
        }
    )
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_tool_pairing()
    text = str(excinfo.value)
    assert "call 'call_1': duplicated call_id" in text
    assert "(first at chain[1])" in text


def test_tool_pairing_detects_tool_message_without_id(tmp_path: Path) -> None:
    records = [
        message("user", uuid="u1", content="跑"),
        message("tool", uuid="t1", parent="u1", content="???"),
    ]
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_tool_pairing()
    assert "tool message without tool_call_id" in str(excinfo.value)


# ── 瞬态记录不落盘 ──────────────────────────────────────────────


@pytest.mark.parametrize("type_", sorted(DELTA_EVENT_TYPES))
def test_transient_records_detected(tmp_path: Path, type_: str) -> None:
    records = linear_history()
    records.append(event("e2", parent="u2", type_=type_, content="delta"))
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_no_transient_records()
    text = str(excinfo.value)
    assert f"transient record type={type_!r}" in text
    assert "line 5" in text
    assert "on active chain" in text


def test_transient_records_detected_off_chain(tmp_path: Path) -> None:
    """链外（被移出链）的瞬态记录同样违规——落盘即违约，不因后续操作消失。"""
    records = linear_history()
    records.append(event("e2", parent="u2", type_="text", text="delta"))
    records.append(message("user", uuid="u3", parent="u2", content="重新开始"))
    view = view_of(tmp_path, records)
    with pytest.raises(HistoryAssertionError) as excinfo:
        view.assert_no_transient_records()
    text = str(excinfo.value)
    assert "transient record type='text' detached (链外)" in text
    assert "uuid=e2" in text


def test_fact_events_are_not_flagged(tmp_path: Path) -> None:
    records = linear_history()
    records.extend(
        [
            event("e2", parent="u2", type_="ask", tool_call_id="call_1"),
            event("e3", parent="e2", type_="interrupted"),
            event("e4", parent="e3", type_="compact_done", original_tokens=1),
        ]
    )
    view = view_of(tmp_path, records)
    view.assert_no_transient_records()
    view.assert_chain_invariants()
