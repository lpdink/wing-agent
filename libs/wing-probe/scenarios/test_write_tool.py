"""Write 工具场景：覆写 / 新建的整机行为（原子替换不改变可见语义，#150）。

被守的语义（`tools/builtin/write.py`）：

- 覆写既有文件：内容整体替换、结果字符串 = ``write: ok (overwritten)`` 的形状
  （旧行数 → 新行数 + 字节数）——模型按它判断写入结果；
- 新建文件：``old_text=None``（全绿 diff）、结果字符串 = ``write: ok (created)``；
- ``diff_content`` 事件：Write 保持**全量**载荷（不做窗口裁剪），old / new 都从
  line 1 起；覆写时 ``old_text`` 是整份旧内容，新建时为 None；
- 原子写（tmp + os.replace）不在 workspace 留下 ``.tmp.*`` 中间文件——这是"写入
  过程不留痕"的清单侧证据（真正的原子性与失败清理由 ``libs/core/tests/`` 下
  ``test_write_tool.py`` / ``test_atomic_write.py`` 的注入测试钉住，probe 层
  不注入失败）。

断言面：workspace 文件（内容 + 清单）、事件时间线（diff_content 逐字段）、落盘链
（diff_content 是 persist=true 事实事件，磁盘记录与广播逐字段一致 + 内置不变量）。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
WRITE_MODEL = "probe/write-atomic"

#: 覆写分支：预置旧内容 → 整份替换。
OVERWRITE_FILE = "overwrite.txt"
OVERWRITE_OLD = "alpha\nbeta\ngamma\n"
OVERWRITE_NEW = "alpha\nBETA\ngamma\ndelta\n"
OVERWRITE_RESULT = "write: ok (overwritten)\n  3 → 4 lines, 23 bytes"

#: 新建分支：目标不存在 → old_text=None。
CREATED_FILE = "created.txt"
CREATED_CONTENT = "fresh content\n"
CREATED_RESULT = "write: ok (created)\n  1 lines, 14 bytes"


@pytest.mark.probe_env(models=[WRITE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_write_overwrite_and_create_keep_payload_semantics(probe: Probe) -> None:
    """覆写 + 新建两轮 Write：内容、结果字符串、diff 载荷与现状一致（行为不变）。

    WHEN 预置 ``overwrite.txt``，剧本先 Write 覆写它、再 Write 新建 ``created.txt``
    THEN 两个文件内容为写入值、结果字符串逐字保持；``diff_content`` 事件携带全量
    old / new（覆写 = 旧内容全文，新建 = None）、start line 均为 1；workspace 清单
    只有这两个文件（无 ``.tmp.*`` 残骸）；落盘链上的 diff 记录与广播一致。
    """
    probe.register(
        WRITE_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall("Write", {"path": OVERWRITE_FILE, "content": OVERWRITE_NEW})
            ]
        ),
        Turn.of(text="overwritten"),
        Turn.of(
            tool_calls=[
                ToolCall("Write", {"path": CREATED_FILE, "content": CREATED_CONTENT})
            ]
        ),
        Turn.of(text="created"),
    )
    # 预置既有文件：覆写分支的起点（Write 之后必须整体替换）。
    (probe.workspace / OVERWRITE_FILE).write_text(OVERWRITE_OLD, encoding="utf-8")

    session = await probe.session(model=WRITE_MODEL)

    first = await session.chat("overwrite the file")
    assert first.data["subtype"] == "success", first.data

    files = probe.files_of(session)
    files.assert_content(OVERWRITE_FILE, equals=OVERWRITE_NEW)

    # ── 第一轮 diff：覆写 = 整份旧内容（不置 None，不裁剪）──
    # 游标随 chat() 推进，回看用全量查询 events()（按 path 过滤锚定本轮）。
    overwrite_diffs = session.watch.events(
        type="diff_content", where={"path": str(probe.workspace / OVERWRITE_FILE)}
    )
    assert len(overwrite_diffs) == 1, session.watch.events(type="diff_content")
    overwrite_diff = overwrite_diffs[0]
    assert overwrite_diff.data["old_text"] == OVERWRITE_OLD, overwrite_diff.data
    assert overwrite_diff.data["new_text"] == OVERWRITE_NEW, overwrite_diff.data
    assert (
        overwrite_diff.data["old_start_line"],
        overwrite_diff.data["new_start_line"],
    ) == (1, 1), overwrite_diff.data

    # 结果字符串：模型可见的写入回执（overwritten 形状 + 行数/字节统计）。
    tool_results = session.watch.events("tool_call_result")
    assert tool_results[0].data["tool_result"] == OVERWRITE_RESULT, tool_results[0].data
    assert tool_results[0].data["tool_success"] is True, tool_results[0].data

    # ── 第二轮：新建文件（old_text=None 全绿）──
    second = await session.chat("create another file")
    assert second.data["subtype"] == "success", second.data
    files.assert_content(CREATED_FILE, equals=CREATED_CONTENT)

    created_diffs = session.watch.events(
        type="diff_content", where={"path": str(probe.workspace / CREATED_FILE)}
    )
    assert len(created_diffs) == 1, session.watch.events(type="diff_content")
    created_diff = created_diffs[0]
    # 新建 = 全绿：old_text 为 null（wire 层剥除 null 键，故按缺省取值断言）。
    assert created_diff.data.get("old_text") is None, created_diff.data
    assert created_diff.data["new_text"] == CREATED_CONTENT, created_diff.data

    tool_results = session.watch.events("tool_call_result")
    assert tool_results[1].data["tool_result"] == CREATED_RESULT, tool_results[1].data

    session.watch.assert_never("error")

    # ── workspace 清单：恰好两个文件，没有 .tmp.* 残骸（原子写不留垃圾）──
    assert files.paths() == [CREATED_FILE, OVERWRITE_FILE], files.paths()

    # ── 落盘：diff 事实事件与广播逐字段一致（resume 重放的源）──
    view = probe.history(session)
    view.assert_chain_invariants()
    persisted = [
        record for record in view.events() if record.get("type") == "diff_content"
    ]
    assert len(persisted) == 2, view.describe()
    by_name = {record["path"]: record for record in persisted}
    overwrite_record = by_name[str(probe.workspace / OVERWRITE_FILE)]
    assert overwrite_record["old_text"] == OVERWRITE_OLD, overwrite_record
    assert overwrite_record["new_text"] == OVERWRITE_NEW, overwrite_record
    created_record = by_name[str(probe.workspace / CREATED_FILE)]
    assert created_record.get("old_text") is None, created_record
    assert created_record["new_text"] == CREATED_CONTENT, created_record
