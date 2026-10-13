"""Experimental hashline 工具场景：content tag 锚点 → 补丁应用 → 过期拒绝 → 寄存器 → MV。

被守的语义（`tools/experimental/`，格式借鉴 oh-my-pi 的 hashline）：

- HashlineRead 的输出携带 4 位十六进制 TAG 与 `N→text` 行号行——TAG 必须与内容
  一起到达模型（锚点机制的前提；所以实验域自带读工具而非改主 Read）；
- HashlineEdit 只接受与磁盘现状对账通过的 TAG：文件变过 → 拒绝、零写入；
- 合法补丁落盘：回执给出**新 TAG**（后续补丁不必重读）、每个操作一条窗口化
  `diff_content` 事件（persist 事实事件，前端 DiffView 的源）；
- 命名寄存器跨调用（会话级）：`CUT @name` 捕获 → 下一次调用粘贴；
- MV 与内容编辑同段：先编辑、后移动，源文件消失、事件仍锚源路径。

断言面：workspace 文件、事件时间线（tool_call_result 回执 / diff_content 逐字段）、
落盘链（persist 事实事件与广播一致）、请求体（工具声明 = 新工具的 LLM 可见名）。
TAG 由场景侧独立实现（LF 规范化 + sha256 前 4 位十六进制）——跨进程锁定契约。
"""

from __future__ import annotations

import hashlib

import pytest

from wing_probe import Probe, ToolCall, Turn

MODEL = "probe/experimental-hashline"

NOTES = "notes.txt"
RENAMED = "renamed.txt"

V1 = "alpha\nbeta\ngamma\n"
#: 补丁 1（PUT 2.=2 → BETA）之后的版本。
V_EDIT1 = "alpha\nBETA\ngamma\n"
#: 补丁 1 之后测试侧的外部改写（制造 stale 场景）。
V2 = "alpha\nBETA\ngamma\ndelta\n"
#: CUT 2.=2（捕获 @keep="BETA"）之后。
V3 = "alpha\ngamma\ndelta\n"
#: 尾部粘贴 @keep 之后。
V4 = "alpha\ngamma\ndelta\nBETA\n"


def tag_of(content: str) -> str:
    """实验工具的 TAG 契约（独立实现）：LF 规范化 + sha256 前 4 位（大写）。"""
    normalized = content.replace("\r\n", "\n").replace("\r", "\n")
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()[:4].upper()


TAG_V1 = tag_of(V1)
TAG_EDIT1 = tag_of(V_EDIT1)
TAG_V2 = tag_of(V2)
TAG_V3 = tag_of(V3)
TAG_V4 = tag_of(V4)


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_experimental_hashline_read_edit_stale_and_registers(
    probe: Probe,
) -> None:
    """读取 → 应用 → 外部改写拒绝 → 重读恢复 → CUT/粘贴 → MV 的整条链路。"""
    probe.register(
        MODEL,
        # ① 读：TAG 随内容返回。
        Turn.of(tool_calls=[ToolCall("HashlineRead", {"path": NOTES})]),
        Turn.of(text="read it"),
        # ② 以 TAG 应用 PUT：替换第 2 行。
        Turn.of(
            tool_calls=[
                ToolCall(
                    "HashlineEdit",
                    {"input": f"[{NOTES}#{TAG_V1}]\nPUT 2.=2:\n+BETA\n"},
                )
            ]
        ),
        Turn.of(text="edited"),
        # ③ 文件已被外部改写 → 旧 TAG 必须被拒绝（零写入）。
        Turn.of(
            tool_calls=[
                ToolCall(
                    "HashlineEdit",
                    {"input": f"[{NOTES}#{TAG_V1}]\nPUT 3.=3:\n+oops\n"},
                )
            ]
        ),
        Turn.of(text="stale refused"),
        # ④ 重读拿到新 TAG。
        Turn.of(tool_calls=[ToolCall("HashlineRead", {"path": NOTES})]),
        Turn.of(text="re-read"),
        # ⑤ CUT 捕获命名寄存器（跨调用持久）。
        Turn.of(
            tool_calls=[
                ToolCall(
                    "HashlineEdit",
                    {"input": f"[{NOTES}#{TAG_V2}]\nCUT 2.=2 @keep\n"},
                )
            ]
        ),
        Turn.of(text="cut"),
        # ⑥ 尾部粘贴寄存器 + 同段 MV。
        Turn.of(
            tool_calls=[
                ToolCall(
                    "HashlineEdit",
                    {"input": f"[{NOTES}#{TAG_V3}]\nPUT >$ @keep\nMV {RENAMED}\n"},
                )
            ]
        ),
        Turn.of(text="pasted and moved"),
    )

    (probe.workspace / NOTES).write_text(V1, encoding="utf-8")
    session = await probe.session(
        model=MODEL, tools=["experimental.Read", "experimental.Edit"]
    )

    workspace_notes = str(probe.workspace / NOTES)

    # ── ① 读：TAG 与行号一起返回 ──
    first = await session.chat("read the notes")
    assert first.data["subtype"] == "success", first.data
    results = session.watch.events("tool_call_result")
    read_result = results[0].data["tool_result"]
    assert read_result.startswith(
        f"[hashline: {workspace_notes}#{TAG_V1} | lines 1-3/3"
    ), read_result
    assert "1→alpha" in read_result and "3→gamma" in read_result
    assert results[0].data["tool_success"] is True, results[0].data

    # ── ② 应用：内容更新 + 回执新 TAG + 窗口事件 ──
    second = await session.chat("replace the second line")
    assert second.data["subtype"] == "success", second.data
    assert (probe.workspace / NOTES).read_text(encoding="utf-8") == V_EDIT1
    results = session.watch.events("tool_call_result")
    assert (
        results[1].data["tool_result"]
        == f"hashline: ok (1 file)\n  {NOTES}: #{TAG_EDIT1}, 1 op, 3 → 3 lines"
    ), results[1].data
    diffs = session.watch.events(type="diff_content", where={"path": workspace_notes})
    assert len(diffs) == 1, session.watch.events(type="diff_content")
    assert diffs[0].data["old_text"] == "alpha\nbeta\ngamma", diffs[0].data
    assert diffs[0].data["new_text"] == "alpha\nBETA\ngamma", diffs[0].data
    assert (diffs[0].data["old_start_line"], diffs[0].data["new_start_line"]) == (1, 1)

    # ── ③ 外部改写 → 旧 TAG 拒绝、零写入、零 diff 事件 ──
    (probe.workspace / NOTES).write_text(V2, encoding="utf-8")
    third = await session.chat("edit with the stale tag")
    assert third.data["subtype"] == "success", third.data
    assert (probe.workspace / NOTES).read_text(encoding="utf-8") == V2
    results = session.watch.events("tool_call_result")
    assert results[2].data["tool_success"] is False, results[2].data
    assert results[2].data["tool_result"] == (
        f"hashline: {NOTES}: stale tag #{TAG_V1}: the file changed since it was "
        "read; re-read it and retry"
    ), results[2].data

    # ── ④ 重读：TAG 跟随新内容 ──
    fourth = await session.chat("re-read the notes")
    assert fourth.data["subtype"] == "success", fourth.data
    results = session.watch.events("tool_call_result")
    assert f"#{TAG_V2} | lines 1-4/4" in results[3].data["tool_result"], results[3].data

    # ── ⑤ CUT 捕获命名寄存器：删除 + 新 TAG ──
    fifth = await session.chat("cut the second line")
    assert fifth.data["subtype"] == "success", fifth.data
    assert (probe.workspace / NOTES).read_text(encoding="utf-8") == V3
    results = session.watch.events("tool_call_result")
    assert (
        results[4].data["tool_result"]
        == f"hashline: ok (1 file)\n  {NOTES}: #{TAG_V3}, 1 op, 4 → 3 lines"
    ), results[4].data

    # ── ⑥ 粘贴寄存器 + MV：内容落位 + 源消失 + 事件仍锚源路径 ──
    sixth = await session.chat("paste and rename")
    assert sixth.data["subtype"] == "success", sixth.data
    assert not (probe.workspace / NOTES).exists()
    assert (probe.workspace / RENAMED).read_text(encoding="utf-8") == V4
    results = session.watch.events("tool_call_result")
    assert results[5].data["tool_result"] == (
        f"hashline: ok (1 file)\n  {NOTES}: #{TAG_V4}, 1 op, 3 → 4 lines, "
        f"moved to {RENAMED}"
    ), results[5].data
    diffs = session.watch.events(type="diff_content", where={"path": workspace_notes})
    assert len(diffs) == 3, session.watch.events(type="diff_content")
    assert diffs[2].data["old_text"] == "alpha\ngamma\ndelta", diffs[2].data
    assert diffs[2].data["new_text"] == "alpha\ngamma\ndelta\nBETA", diffs[2].data

    # ── 请求体：工具声明 = 新工具的 LLM 可见名（命名域引用解析的端到端证据）──
    ctx0 = probe.context(MODEL, 0)
    assert sorted(ctx0.tool_names) == ["HashlineEdit", "HashlineRead"], ctx0.tool_names

    session.watch.assert_never("error")

    # ── 落盘：diff 是 persist 事实事件（广播与磁盘逐字段一致）──
    view = probe.history(session)
    view.assert_chain_invariants()
    persisted = [r for r in view.events() if r.get("type") == "diff_content"]
    assert len(persisted) == 3, view.describe()
