"""红线过渡断言的构造样例对账（tasks 5.6）。

对照用例成对出现：**正确形状 → 通过**，**错误形状 → 失败且失败信息含定位**
（守护 compact / rewind / fork 的"上下文不变量"，静默退化不可接受）。

形状来源（只读参考产品实现，不 import）：

- compact：``ContextManager.do_manual_compact``（新链 = 压缩节点，整窗压缩）与
  ``_apply_pending_compact``（压缩节点 + 重链接 tail），runtime 随后落
  ``compact_done`` 事实事件；
- rewind：复制行 = target 的最近 Message 祖先内容 + 祖父 parent + 全新 uuid；
- fork：``extract_subchain``（walk_full_chain 前缀）+ ``_remap_chain_uuids``
  （uuid / parent_uuid / unzip 全量重映射）+ fork metadata 快照。
"""

from __future__ import annotations

import copy
import json
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import pytest

from wing_probe.history import (
    HistoryAssertionError,
    HistoryView,
    assert_compact_transition,
    assert_fork_of,
    assert_rewind_transition,
    fork_metadata,
)

TS = "2026-01-01T00:00:00"
SOURCE_SESSION = "sess-source"


# ── 构造工具（与 test_history_view.py 同口径，保持产品落盘形状） ──


def _prune(value: Any) -> Any:
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
    usage: dict[str, Any] | None = None,
    unzip: str | None = None,
) -> dict[str, Any]:
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
    else:
        if content is not None:
            record["content"] = content
        if tool_call_id:
            record["tool_call_id"] = tool_call_id
    if usage is not None:
        record["usage"] = usage
    return _prune(record)


def event(uuid: str, *, parent: str | None, type_: str, **extra: Any) -> dict[str, Any]:
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


def write_session(
    root: Path,
    session_id: str,
    records: Sequence[Mapping[str, Any]],
    metadata: Mapping[str, Any] | None = None,
) -> Path:
    session_dir = root / session_id
    session_dir.mkdir(parents=True, exist_ok=True)
    if records:
        lines = [json.dumps(dict(record), ensure_ascii=False) for record in records]
        (session_dir / "history.jsonl").write_text(
            "\n".join(lines) + "\n", encoding="utf-8"
        )
    if metadata is not None:
        (session_dir / "metadata.json").write_text(
            json.dumps(dict(metadata), ensure_ascii=False), encoding="utf-8"
        )
    return session_dir


def view_of(root: Path, session_id: str) -> HistoryView:
    return HistoryView(root / session_id)


def remap(
    records: Sequence[Mapping[str, Any]], mapping: Mapping[str, str]
) -> list[dict[str, Any]]:
    """模拟 ``_remap_chain_uuids``：深拷贝 + uuid / parent_uuid / unzip 重映射。"""
    clones = copy.deepcopy([dict(record) for record in records])
    for clone in clones:
        uuid = clone.get("uuid")
        if isinstance(uuid, str):
            clone["uuid"] = mapping.get(uuid, uuid)
        parent = clone.get("parent_uuid")
        if isinstance(parent, str):
            clone["parent_uuid"] = mapping.get(parent, parent)
        unzip = clone.get("unzip_last_uuid")
        if isinstance(unzip, str):
            clone["unzip_last_uuid"] = mapping.get(unzip, unzip)
    return clones


# ── compact ─────────────────────────────────────────────────────


def compact_before() -> list[dict[str, Any]]:
    """多轮历史（含事件混排 + 工具往返）。"""
    return [
        message("user", uuid="u1", content="任务开始"),
        message(
            "assistant", uuid="a1", parent="u1", content="收到", reasoning="想一下"
        ),
        event("e1", parent="a1", type_="diff_content", path="a.py"),
        message("user", uuid="u2", parent="e1", content="跑一下"),
        message(
            "assistant",
            uuid="a2",
            parent="u2",
            tool_calls=[call("call_1", "Bash", {"command": "ls"})],
        ),
        message("tool", uuid="t1", parent="a2", content="a.txt", tool_call_id="call_1"),
        message("assistant", uuid="a3", parent="t1", content="完成"),
    ]


def manual_compact_after(before: Sequence[Mapping[str, Any]]) -> list[dict[str, Any]]:
    """手动 compact：整窗压成一条压缩节点 + runtime 的 compact_done 事件。"""
    return [
        *[dict(record) for record in before],
        message("assistant", uuid="c1", content="[Compact] 摘要：进行中", unzip="a3"),
        event("cd1", parent="c1", type_="compact_done", original_tokens=100),
    ]


def background_compact_after(
    before: Sequence[Mapping[str, Any]],
) -> list[dict[str, Any]]:
    """后台 apply：压缩 [u1..u2]，保留区 [a2, t1, a3] 换新 uuid 重链接。"""
    return [
        *[dict(record) for record in before],
        message("assistant", uuid="c1", content="[Compact] 摘要：进行中", unzip="u2"),
        *relink(relinked_tail(), parent="c1"),
    ]


def relinked_tail(*, last_content: str = "完成") -> list[dict[str, Any]]:
    """保留区的重链接副本（新 uuid x1/x2/x3，内容与 before 保留段逐字段等价）。"""
    return [
        message(
            "assistant",
            uuid="x1",
            tool_calls=[call("call_1", "Bash", {"command": "ls"})],
        ),
        message("tool", uuid="x2", tool_call_id="call_1", content="a.txt"),
        message("assistant", uuid="x3", content=last_content),
    ]


def relink(tail: Sequence[Mapping[str, Any]], *, parent: str) -> list[dict[str, Any]]:
    """把保留区节点重链接到 ``parent`` 之后（parent 依次重接）。"""
    linked: list[dict[str, Any]] = []
    previous = parent
    for record in tail:
        clone = copy.deepcopy(dict(record))
        clone["parent_uuid"] = previous
        previous = str(clone["uuid"])
        linked.append(clone)
    return linked


# 注意：上面的 x1/x2/x3 是手工构造的重链接 tail；用 relink 复核 parent 依次重接
def test_relink_helper_is_consistent() -> None:
    tail = [
        message("assistant", uuid="x1", content="a"),
        message("assistant", uuid="x2", content="b"),
    ]
    linked = relink(tail, parent="c1")
    assert [record["parent_uuid"] for record in linked] == ["c1", "x1"]


def test_compact_transition_manual_shape(tmp_path: Path) -> None:
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(tmp_path, "s1", manual_compact_after(before_records))
    after = view_of(tmp_path, "s1")

    material = assert_compact_transition(before, after)
    assert material["compact_uuid"] == "c1"
    assert material["unzip_last_uuid"] == "a3"
    assert material["compressed_messages"] == 6  # 活跃链上的全部 Message
    assert material["tail_uuids"] == []
    assert material["trailing_uuids"] == ["cd1"]
    before.assert_chain_invariants()
    after.assert_chain_invariants()
    after.assert_tool_pairing()


def test_compact_transition_background_apply_shape(tmp_path: Path) -> None:
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(tmp_path, "s1", background_compact_after(before_records))
    after = view_of(tmp_path, "s1")

    material = assert_compact_transition(before, after)
    assert material["unzip_last_uuid"] == "u2"
    assert material["compressed_messages"] == 3  # u1 / a1 / u2
    assert material["tail_source_uuids"] == ["a2", "t1", "a3"]
    assert material["tail_uuids"] == ["x1", "x2", "x3"]
    after.assert_chain_invariants()
    after.assert_tool_pairing()


def test_compact_transition_rejects_lost_tail_message(tmp_path: Path) -> None:
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="c1", content="[Compact] 摘要", unzip="u2"),
            *relink(relinked_tail()[:-1], parent="c1"),  # 丢掉 "完成"
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    text = str(excinfo.value)
    assert "shorter than" in text
    assert "after relinked tail" in text
    assert "uuid=a3" in text  # 缺失的保留区消息（before 侧）被点名


def test_compact_transition_rejects_content_drift_in_tail(tmp_path: Path) -> None:
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="c1", content="[Compact] 摘要", unzip="u2"),
            *relink(relinked_tail(last_content="完成（改写）"), parent="c1"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    text = str(excinfo.value)
    assert "after relinked tail[2]" in text
    assert "content: expected '完成' got '完成（改写）'" in text


def test_compact_transition_rejects_uuid_reuse(tmp_path: Path) -> None:
    """保留区复用旧 uuid（未重链接）——resume / 重放会把两条记录当成同一条。"""
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    tail = [dict(before_records[4]), dict(before_records[5]), dict(before_records[6])]
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="c1", content="[Compact] 摘要", unzip="u2"),
            *relink(tail, parent="c1"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    assert "reuses uuid 'a2'" in str(excinfo.value)


def test_compact_transition_rejects_unknown_unzip(tmp_path: Path) -> None:
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="c1", content="[Compact] 摘要", unzip="ghost"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    assert "unzip_last_uuid='ghost' is not in the before record set" in str(
        excinfo.value
    )


def test_compact_transition_rejects_missing_prefix(tmp_path: Path) -> None:
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="c1", content="摘要没有前缀", unzip="a3"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    assert "content must start with '[Compact]'" in str(excinfo.value)


def test_compact_transition_rejects_deleted_before_node(tmp_path: Path) -> None:
    """compact 是 append-only：旧节点必须仍在 records 里。"""
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    kept = [record for record in before_records if record["uuid"] != "e1"]
    write_session(
        tmp_path,
        "s1",
        [
            *kept,
            message("assistant", uuid="c1", content="[Compact] 摘要", unzip="a3"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    assert "disappeared from after.records" in str(excinfo.value)
    assert "'e1'" in str(excinfo.value)


def test_compact_transition_rejects_parented_compact_node(tmp_path: Path) -> None:
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message(
                "assistant",
                uuid="c1",
                parent="a3",
                content="[Compact] 摘要",
                unzip="a3",
            ),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    text = str(excinfo.value)
    assert "compact node must have no parent_uuid" in text
    assert "is not the root of the after active chain" in text


def test_compact_transition_trailing_message_is_strict_by_default(
    tmp_path: Path,
) -> None:
    """保留区之后的计划外 Message：默认严格失败，显式放行后通过。"""
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    after_records = manual_compact_after(before_records)
    after_records.append(message("user", uuid="n1", parent="cd1", content="后续提问"))
    write_session(tmp_path, "s1", after_records)
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_compact_transition(before, after)
    assert "unexpected extra Message node" in str(excinfo.value)
    material = assert_compact_transition(before, after, allow_trailing=True)
    assert material["trailing_uuids"] == ["cd1", "n1"]


# ── rewind ──────────────────────────────────────────────────────


def rewind_before() -> list[dict[str, Any]]:
    return [
        message("user", uuid="u1", content="第一问"),
        message("assistant", uuid="a1", parent="u1", content="第一答"),
        message("user", uuid="u2", parent="a1", content="第二问"),
        message("assistant", uuid="a2", parent="u2", content="第二答"),
    ]


def test_rewind_transition_copy_shape(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="r1", parent="u1", content="第一答"),
        ],
    )
    after = view_of(tmp_path, "s1")

    material = assert_rewind_transition(before, after, "u2")
    assert material["mode"] == "copy"
    assert material["copy_uuid"] == "r1"
    assert material["source_uuid"] == "a1"
    assert material["expected_draft"] == "第二问"  # API draft 应等于它
    assert material["dropped_uuids"] == ["u2", "a2"]
    assert [record["uuid"] for record in after.active_chain()] == ["u1", "r1"]
    after.assert_chain_invariants()
    after.assert_tool_pairing()


def test_rewind_transition_skips_event_nodes(tmp_path: Path) -> None:
    """target 的直接 parent 是事件记录 → 复制源取最近 Message。"""
    before_records = [
        message("user", uuid="u1", content="第一问"),
        message("assistant", uuid="a1", parent="u1", content="第一答"),
        event("e1", parent="a1", type_="diff_content", path="a.py"),
        message("user", uuid="u2", parent="e1", content="第二问"),
    ]
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="r1", parent="u1", content="第一答"),
        ],
    )
    after = view_of(tmp_path, "s1")
    material = assert_rewind_transition(before, after, "u2")
    assert material["source_uuid"] == "a1"
    assert material["skipped_event_uuids"] == ["e1"]
    assert [record["uuid"] for record in after.active_chain()] == ["u1", "r1"]


def test_rewind_transition_to_root_shape(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("system", uuid="r0", content="[rewind_to_root]"),
        ],
    )
    after = view_of(tmp_path, "s1")
    material = assert_rewind_transition(before, after, "u1")
    assert material["mode"] == "root"
    assert material["source_uuid"] is None
    assert material["expected_draft"] == "第一问"
    assert material["dropped_uuids"] == ["u1", "a1", "u2", "a2"]
    assert [record["uuid"] for record in after.active_chain()] == ["r0"]


def test_rewind_transition_to_root_rejects_wrong_shape(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="r0", parent="u1", content="[rewind_to_root]"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "u1")
    text = str(excinfo.value)
    assert "role must be 'system' for a rewind-to-root" in text
    assert "must not have a parent_uuid" in text


def test_rewind_transition_current_is_noop(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(tmp_path, "s1", before_records)
    after = view_of(tmp_path, "s1")
    material = assert_rewind_transition(before, after, "current")
    assert material["mode"] == "noop"
    assert material["expected_draft"] is None

    write_session(
        tmp_path,
        "s1",
        [*before_records, message("user", uuid="z1", parent="a2", content="新消息")],
    )
    changed = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, changed, "current")
    assert "active chain changed" in str(excinfo.value)


def test_rewind_transition_rejects_wrong_copy_source(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("user", uuid="r1", parent="u1", content="第一问"),  # 取错源
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "u2")
    text = str(excinfo.value)
    assert "copy node" in text
    assert "role: expected 'assistant' got 'user'" in text
    assert "uuid=a1" in text  # 期望的复制源被点名


def test_rewind_transition_rejects_copy_content_drift(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="r1", parent="u1", content="第一答（改写）"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "u2")
    assert "content: expected '第一答' got '第一答（改写）'" in str(excinfo.value)


def test_rewind_transition_rejects_wrong_parent(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="r1", parent="a1", content="第一答"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "u2")
    text = str(excinfo.value)
    assert "!= grandparent 'u1'" in text
    assert "复制行的 parent 指向 target 的祖父" in text


def test_rewind_transition_rejects_uuid_reuse(tmp_path: Path) -> None:
    """复制行复用 before 里已存在的 uuid —— 链身份被共享。"""
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="a2", parent="u1", content="第一答"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "u2")
    text = str(excinfo.value)
    assert "reuses uuid 'a2' from before records" in text
    assert "复制行必须是全新 uuid" in text


def test_rewind_transition_rejects_target_left_on_chain(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            *before_records,
            message("assistant", uuid="r1", parent="u2", content="第二答"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "u2")
    text = str(excinfo.value)
    assert "is still on the after active chain" in text
    assert "前缀链不一致" in text


def test_rewind_transition_rejects_deleted_nodes(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(
        tmp_path,
        "s1",
        [
            message("user", uuid="u1", content="第一问"),
            message("assistant", uuid="a1", parent="u1", content="第一答"),
            message("assistant", uuid="r1", parent="u1", content="第一答"),
        ],
    )
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "u2")
    assert "disappeared from after.records" in str(excinfo.value)


def test_rewind_transition_unknown_target(tmp_path: Path) -> None:
    before_records = rewind_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, before, "ghost")
    assert "target uuid 'ghost' not found in before records" in str(excinfo.value)


def test_rewind_transition_rejects_event_target(tmp_path: Path) -> None:
    before_records = rewind_before()
    before_records.insert(1, event("e1", parent="u1", type_="diff_content"))
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, before, "e1")
    assert "is an event record, not a Message" in str(excinfo.value)


def test_rewind_transition_rejects_dangling_parent_chain(tmp_path: Path) -> None:
    before_records = [
        message("user", uuid="u1", content="第一问"),
        message("assistant", uuid="a1", parent="ghost", content="第一答"),
    ]
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    after_records = [*before_records, message("assistant", uuid="r1", parent="ghost")]
    write_session(tmp_path, "s1", after_records)
    after = view_of(tmp_path, "s1")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_rewind_transition(before, after, "a1")
    assert "is not in the before record set" in str(excinfo.value)
    assert "链断裂" in str(excinfo.value)


# ── fork ────────────────────────────────────────────────────────


def fork_source() -> list[dict[str, Any]]:
    return [
        message("user", uuid="u1", content="第一问"),
        message("assistant", uuid="a1", parent="u1", content="第一答", reasoning="想"),
        event("e1", parent="a1", type_="diff_content", path="a.py"),
        message("user", uuid="u2", parent="e1", content="第二问"),
        message("assistant", uuid="a2", parent="u2", content="第二答"),
    ]


SOURCE_METADATA: dict[str, Any] = {
    "workspace": "/tmp/ws",
    "template_name": "default",
    "model_name": "probe/basic",
    "provider_name": "probe-unit",
    "last_interaction": TS,
}

FORK_UUID_MAP = {"u1": "cu1", "a1": "ca1", "e1": "ce1"}


def child_metadata(**overrides: Any) -> dict[str, Any]:
    data: dict[str, Any] = {
        "forked_from": SOURCE_SESSION,
        "workspace": "/tmp/ws",
        "template_name": "default",
        "model_name": "probe/basic",
        "provider_name": "probe-unit",
        "last_interaction": TS,
    }
    data.update(overrides)
    return data


def test_fork_prefix_and_metadata(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    child_records = remap(source_records[:3], FORK_UUID_MAP)
    write_session(tmp_path, "sess-child", child_records, child_metadata())
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")

    material = assert_fork_of(source, child, "u2")
    assert material["expected_draft"] == "第二问"
    assert material["uuid_map"] == FORK_UUID_MAP
    assert material["chain_length"] == 3
    assert material["child_metadata"]["forked_from"] == SOURCE_SESSION
    assert [record["uuid"] for record in child.active_chain()] == ["cu1", "ca1", "ce1"]
    child.assert_chain_invariants()
    child.assert_tool_pairing()


def test_fork_current_copies_whole_chain(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    mapping = {"u1": "cu1", "a1": "ca1", "e1": "ce1", "u2": "cu2", "a2": "ca2"}
    write_session(
        tmp_path, "sess-child", remap(source_records, mapping), child_metadata()
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    material = assert_fork_of(source, child, "current")
    assert material["expected_draft"] == ""
    assert material["chain_length"] == 5


def test_fork_without_metadata_check(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records)
    write_session(tmp_path, "sess-child", remap(source_records[:3], FORK_UUID_MAP))
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    assert_fork_of(source, child, "u2", check_metadata=False)


def test_fork_rejects_short_chain(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(
        tmp_path,
        "sess-child",
        remap(source_records[:2], FORK_UUID_MAP),
        child_metadata(),
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    text = str(excinfo.value)
    assert "shorter than" in text
    assert "uuid=e1" in text  # 缺失的期望节点（source 侧的事件）被点名


def test_fork_rejects_uuid_intersection(tmp_path: Path) -> None:
    """未重映射（uuid 与源相同）——分叉会与源共享链身份。"""
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(
        tmp_path,
        "sess-child",
        copy.deepcopy(source_records[:3]),
        child_metadata(),
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    assert "child uuids intersect the source record set" in str(excinfo.value)
    assert "'a1'" in str(excinfo.value)


def test_fork_rejects_missing_event(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    dropped = [source_records[0], source_records[1]]  # 事件 e1 丢失
    mapping = {"u1": "cu1", "a1": "ca1"}
    write_session(tmp_path, "sess-child", remap(dropped, mapping), child_metadata())
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    text = str(excinfo.value)
    assert "event records did not follow the fork" in text
    assert "'diff_content'" in text


def test_fork_rejects_broken_parent_relation(tmp_path: Path) -> None:
    """子链 parent 关系被破坏 → 链不再是 source 前缀的等价副本。

    链是 parent 闭包，破坏 parent 关系必然以"链变短 / 前缀不等价"的形式暴露
    （断言里另有一条 v.s. 逐节点 parent 衔接的复核，见 invariants.py）。
    """
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    child_records = remap(source_records[:3], FORK_UUID_MAP)
    child_records[1]["parent_uuid"] = None  # ca1 的 parent 被抹掉
    write_session(tmp_path, "sess-child", child_records, child_metadata())
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    text = str(excinfo.value)
    assert "shorter than" in text
    assert "child chain (2 node(s))" in text
    assert "uuid=ca1" in text  # 断链处的节点被点名
    # 链是 parent 闭包：破坏 parent 关系的症状以"子链不再是 source 前缀的等价副本"
    # 呈现（前缀不等价 + 链变短），断言即按此对账。


def test_fork_rejects_content_drift(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    child_records = remap(source_records[:3], FORK_UUID_MAP)
    child_records[0]["content"] = "第一问（改写）"
    write_session(tmp_path, "sess-child", child_records, child_metadata())
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    assert "content: expected '第一问' got '第一问（改写）'" in str(excinfo.value)


def test_fork_metadata_missing_field(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(
        tmp_path,
        "sess-child",
        remap(source_records[:3], FORK_UUID_MAP),
        child_metadata(workspace=None),
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    text = str(excinfo.value)
    assert "missing required field(s) ['workspace']" in text
    assert "forked_from" in text  # 报告里给出实际 metadata


def test_fork_metadata_missing_file(tmp_path: Path) -> None:
    write_session(tmp_path, SOURCE_SESSION, fork_source(), SOURCE_METADATA)
    write_session(tmp_path, "sess-child", [])
    with pytest.raises(HistoryAssertionError) as excinfo:
        fork_metadata(tmp_path / "sess-child")
    assert "metadata.json missing" in str(excinfo.value)


def test_fork_metadata_pure_parse(tmp_path: Path) -> None:
    write_session(tmp_path, "sess-child", [], {"forked_from": "x"})
    assert fork_metadata(tmp_path / "sess-child", require=()) == {"forked_from": "x"}


def test_fork_rejects_forked_from_mismatch(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(
        tmp_path,
        "sess-child",
        remap(source_records[:3], FORK_UUID_MAP),
        child_metadata(forked_from="someone-else"),
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    text = str(excinfo.value)
    assert "fork metadata snapshot violated" in text
    assert "forked_from: expected 'sess-source' got 'someone-else'" in text


def test_fork_rejects_model_snapshot_mismatch(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(
        tmp_path,
        "sess-child",
        remap(source_records[:3], FORK_UUID_MAP),
        child_metadata(model_name="probe/other"),
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    text = str(excinfo.value)
    assert (
        "metadata model_name: expected 'probe/basic' (fork 时刻快照 = 源当前值) "
        "got 'probe/other'" in text
    )


def test_fork_trailing_message_is_strict_by_default(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    child_records = remap(source_records[:3], FORK_UUID_MAP)
    child_records.append(
        message("user", uuid="cu2", parent="ce1", content="子会话继续")
    )
    write_session(tmp_path, "sess-child", child_records, child_metadata())
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    assert "unexpected extra Message node" in str(excinfo.value)
    material = assert_fork_of(source, child, "u2", allow_trailing=True)
    assert material["trailing_uuids"] == ["cu2"]


def test_fork_allows_child_turns_after_the_fork(tmp_path: Path) -> None:
    """子 session 在 fork 之后继续跑（自产 Message 与事件）——不算"事件没随行"。"""
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    child_records = remap(source_records[:3], FORK_UUID_MAP)
    child_records.extend(
        [
            message(
                "assistant",
                uuid="cx1",
                parent="ce1",
                tool_calls=[call("call_9", "Bash", {"command": "ls"})],
            ),
            event("cx2", parent="cx1", type_="turn_result", subtype="success"),
            message(
                "tool", uuid="cx3", parent="cx2", content="ok", tool_call_id="call_9"
            ),
        ]
    )
    write_session(tmp_path, "sess-child", child_records, child_metadata())
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    material = assert_fork_of(source, child, "u2", allow_trailing=True)
    assert material["prefix_length"] == 3  # 前缀语义长度（含随行事件）
    assert material["chain_length"] == 6  # 子链含 fork 之后的演进
    assert material["trailing_uuids"] == ["cx1", "cx2", "cx3"]
    child.assert_tool_pairing()


def test_fork_snapshot_fields_can_be_narrowed(tmp_path: Path) -> None:
    """源侧切换过模型 → 子侧快照与源当前值不等，可显式收窄对账面。"""
    source_records = fork_source()
    source_meta = dict(SOURCE_METADATA)
    source_meta["model_name"] = "probe/other"  # fork 之后源切换了模型
    write_session(tmp_path, SOURCE_SESSION, source_records, source_meta)
    write_session(
        tmp_path,
        "sess-child",
        remap(source_records[:3], FORK_UUID_MAP),
        child_metadata(),
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "u2")
    assert "metadata model_name" in str(excinfo.value)
    material = assert_fork_of(source, child, "u2", snapshot_fields=())
    assert material["child_metadata"]["model_name"] == "probe/basic"


def test_fork_reports_unverifiable_snapshot_fields(tmp_path: Path) -> None:
    """源 metadata 无该字段记录（从未切换过模型）→ 如实报告"无法对账"。"""
    source_records = fork_source()
    source_meta = {
        "workspace": SOURCE_METADATA["workspace"],
        "template_name": "default",
    }
    write_session(tmp_path, SOURCE_SESSION, source_records, source_meta)
    write_session(
        tmp_path,
        "sess-child",
        remap(source_records[:3], FORK_UUID_MAP),
        child_metadata(),
    )
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    material = assert_fork_of(source, child, "u2")
    assert material["unverifiable_metadata_fields"] == ["model_name", "provider_name"]


def test_fork_unknown_target(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(tmp_path, "sess-child", [])
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "ghost")
    assert "not found in source records" in str(excinfo.value)


def test_fork_rejects_event_target(tmp_path: Path) -> None:
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(tmp_path, "sess-child", [])
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    with pytest.raises(HistoryAssertionError) as excinfo:
        assert_fork_of(source, child, "e1")
    assert "is an event record, not a Message" in str(excinfo.value)


def test_fork_from_root_produces_empty_child(tmp_path: Path) -> None:
    """fork 根消息 → 子链接空前缀（父为空），metadata 仍须写全。"""
    source_records = fork_source()
    write_session(tmp_path, SOURCE_SESSION, source_records, SOURCE_METADATA)
    write_session(tmp_path, "sess-child", [], child_metadata())
    source = view_of(tmp_path, SOURCE_SESSION)
    child = view_of(tmp_path, "sess-child")
    material = assert_fork_of(source, child, "u1")
    assert material["chain_length"] == 0
    assert material["expected_draft"] == "第一问"


# ── 与不变量联动：过渡断言通过后，不变量仍应成立 ──────────────────


def test_transitions_keep_invariants_green(tmp_path: Path) -> None:
    """三类操作的正确形状在两视图上均通过全部内置不变量。"""
    before_records = compact_before()
    write_session(tmp_path, "s1", before_records)
    before = view_of(tmp_path, "s1")
    write_session(tmp_path, "s1", manual_compact_after(before_records))
    after = view_of(tmp_path, "s1")
    for view in (before, after):
        view.assert_chain_invariants()
        view.assert_tool_pairing()
        view.assert_no_transient_records()
