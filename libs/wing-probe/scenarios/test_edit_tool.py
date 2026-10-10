"""Edit 工具场景：写入目标政策与 Write 同一份（`tools/internal/write_target.py`）。

被守的语义（`tools/builtin/edit.py`）：

- **符号链接**目标：编辑**真实目标**、链接保留——否则「编辑成功」而真实文件
  静默留着旧内容（原子替换会把链接本身换成普通文件）；
- **只读**目标（0444）：拒绝（``edit: write failed: ... Permission denied``），
  文件内容与权限位原样——rename 只需要目录写权限，不加显式拒绝就能绕过只读位；
- **常规文件**：编辑成功（``edit: ok @ line N``）——对照组，确认政策没有误伤主路径。

断言面：workspace 文件（内容 + 类型 + 权限位）、事件时间线（tool_call_result 的
模型可见回执与 success 标记）。
"""

from __future__ import annotations

import stat

import pytest

from wing_probe import Probe, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
EDIT_MODEL = "probe/edit-target-policy"

#: 符号链接分支：link.txt → real.txt，编辑必须落到 real.txt。
REAL_FILE = "real.txt"
LINK_FILE = "link.txt"
LINK_OLD = "old value\n"
LINK_NEW = "new value\n"

#: 只读分支：0444 文件编辑被拒。
READONLY_FILE = "readonly.txt"
READONLY_CONTENT = "protected"

#: 对照分支：常规文件照常编辑。
PLAIN_FILE = "plain.txt"


@pytest.mark.probe_env(models=[EDIT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_edit_target_policy_matches_write(probe: Probe) -> None:
    """链接写穿 + 只读拒绝 + 常规成功：三轮 Edit 的目标政策逐类对账。

    WHEN workspace 里预置「链接 / 只读文件 / 常规文件」，剧本依次 Edit 三者
    THEN 链接分支：真实目标被改、链接仍是链接；只读分支：拒绝（回执含
    ``Permission denied``、success=False、内容与权限位原样）；常规分支：内容
    更新、回执为成功形状。
    """
    probe.register(
        EDIT_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall(
                    "Edit",
                    {
                        "path": LINK_FILE,
                        "old_string": "old value",
                        "new_string": "new value",
                    },
                )
            ]
        ),
        Turn.of(text="link edited"),
        Turn.of(
            tool_calls=[
                ToolCall(
                    "Edit",
                    {
                        "path": READONLY_FILE,
                        "old_string": "protected",
                        "new_string": "hacked",
                    },
                )
            ]
        ),
        Turn.of(text="readonly refused"),
        Turn.of(
            tool_calls=[
                ToolCall(
                    "Edit",
                    {
                        "path": PLAIN_FILE,
                        "old_string": "old value",
                        "new_string": "new value",
                    },
                )
            ]
        ),
        Turn.of(text="plain edited"),
    )

    # 预置：真实目标 + 指向它的链接 / 只读文件 / 常规文件。
    real = probe.workspace / REAL_FILE
    real.write_text(LINK_OLD, encoding="utf-8")
    (probe.workspace / LINK_FILE).symlink_to(real)
    readonly = probe.workspace / READONLY_FILE
    readonly.write_text(READONLY_CONTENT, encoding="utf-8")
    readonly.chmod(0o444)
    (probe.workspace / PLAIN_FILE).write_text(LINK_OLD, encoding="utf-8")

    session = await probe.session(model=EDIT_MODEL)

    # ── 第一轮：符号链接 → 编辑真实目标，链接保留 ──
    first = await session.chat("edit through the link")
    assert first.data["subtype"] == "success", first.data
    assert (probe.workspace / LINK_FILE).is_symlink()
    assert real.read_text(encoding="utf-8") == LINK_NEW
    results = session.watch.events("tool_call_result")
    assert results[0].data["tool_success"] is True, results[0].data
    assert results[0].data["tool_result"].startswith("edit: ok @ line 1"), results[
        0
    ].data

    # ── 第二轮：只读目标 → 拒绝，内容与权限位原样 ──
    second = await session.chat("edit the readonly file")
    assert second.data["subtype"] == "success", second.data
    assert readonly.read_text(encoding="utf-8") == READONLY_CONTENT
    assert stat.S_IMODE(readonly.stat().st_mode) == 0o444
    results = session.watch.events("tool_call_result")
    assert results[1].data["tool_success"] is False, results[1].data
    assert "Permission denied" in results[1].data["tool_result"], results[1].data

    # ── 第三轮：常规文件 → 照常编辑（政策没有误伤主路径）──
    third = await session.chat("edit the plain file")
    assert third.data["subtype"] == "success", third.data
    assert (probe.workspace / PLAIN_FILE).read_text(encoding="utf-8") == LINK_NEW
    results = session.watch.events("tool_call_result")
    assert results[2].data["tool_success"] is True, results[2].data

    session.watch.assert_never("error")
    probe.history(session).assert_chain_invariants()
