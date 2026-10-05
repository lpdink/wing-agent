"""工具热切换红线场景（spec「工具热切换」）。

被守的语义（`libs/core/wing/context_manager.py` 的 `on_tools_changed` + 注入的
System Reminder，策略归 CM）：

- **链非空（热切换）= 冻结声明集 + 注入 reminder**：`POST /api/session/update`
  改 `tools` 后，LLM 可见的 `tools` 声明**保持旧集**（KV cache 前缀不碎），而
  模型被告知新工具（链上追加一条 user 角色的 System Reminder，带允许/禁止清单
  与 `<tools>` 全量 schema）。**可执行集立刻更新**——reminder 声明可用的新工具
  真的能被调用（冻结的是"声明"，不是"能力"）；
- **链空（冷路径）= 直接切换**：没有前缀要保护，声明集直接换成新集，**不注入**
  reminder；
- **compact 后同步**：压缩已打破前缀缓存，声明集随可执行集更新（reminder 也随
  被压缩区间离开上下文）；
- **重建后跟随可执行集**（`reset_declared_tools` 的语义）：逐出 → 水合重建 CM 时
  声明集**不是**被冻结的旧集，而是当时的可执行集——否则重启后请求前缀会凭空多
  一条 reminder / 少一段声明。

断言面是**假 Provider 留档的请求体**（`probe.context(model, i)`：`tool_names` 与
消息文本）+ 落盘链 + workspace 文件——不看内部状态，重构改坏就一定变红。
"""

from __future__ import annotations

import json

import pytest

from wing_probe import ContextView, Probe, Session, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
HOT_MODEL = "probe/tool-switch-hot"
COLD_MODEL = "probe/tool-switch-cold"
COMPACT_MODEL = "probe/tool-switch-compact"
REBUILD_MODEL = "probe/tool-switch-rebuild"
REMOVAL_MODEL = "probe/tool-switch-removal"

#: 初始集 / 扩展集（声明集与可执行集的差异就是这条红线的全部素材）。
INITIAL_TOOLS = ("Bash", "Read")
EXTENDED_TOOLS = ("Bash", "Read", "Write")
#: 只减不加的目标集（reminder 的 ``Removed:`` 分支）。
REMAINING_TOOLS = ("Read",)

#: reminder 的固定锚点（`ContextManager._inject_tool_change_reminder`）。
REMINDER_HEADER = "[System Reminder] Your available tools have changed."
REMINDER_OPEN = "<tools>"
REMINDER_CLOSE = "</tools>"

#: 工具调用写入的文件（"冻结声明集 ≠ 冻结可执行集" 的 workspace 证据）。
HOT_FILE = "hot-switch.txt"
HOT_CONTENT = "written-through-hot-switch"
#: 被移除的工具若被执行会写出的文件（断言它没被写出来）。
REMOVED_FILE = "removed-tool.txt"

#: 摘要产物（假 Provider 的压缩调用返回带 `<summary>` 的文本）。
SUMMARY_TEXT = "Task: keep the tool declaration frozen. Next: continue."


def _reminder_of(session_context: ContextView) -> str:
    """请求上下文里**唯一**的 System Reminder 正文（缺失 / 多条即报错）。"""
    found = [
        message
        for message in session_context.messages
        if REMINDER_HEADER in (message.content or "")
    ]
    assert len(found) == 1, (
        f"expected exactly one System Reminder in the request, got {len(found)}\n"
        f"{session_context.describe()}"
    )
    return found[0].content


def _reminder_tool_names(content: str) -> list[str]:
    """解析 reminder 的 ``<tools>`` 段：逐行 JSON schema → 工具名（按出现序）。

    reminder 的 schema 是 `Tool.to_openai()` 的逐行 dump——这是"全量 schema"
    断言的唯一依据：名字对不上（少一个 / 多一个）就说明注入的内容与当前可执行集
    不一致。
    """
    body = content.split(REMINDER_OPEN, 1)
    assert len(body) == 2, f"reminder has no {REMINDER_OPEN} block:\n{content}"
    schema_lines = body[1].split(REMINDER_CLOSE, 1)
    assert len(schema_lines) == 2, f"reminder has no {REMINDER_CLOSE}:\n{content}"
    names: list[str] = []
    for line in schema_lines[0].splitlines():
        entry = line.strip()
        if not entry:
            continue
        parsed = json.loads(entry)
        assert parsed["type"] == "function", parsed
        assert "parameters" in parsed["function"], parsed
        names.append(parsed["function"]["name"])
    return names


def _reminder_texts(session_context: ContextView) -> list[str]:
    """请求里出现过的 System Reminder 文本（"只有一条" 类断言用）。"""
    return [
        message.content
        for message in session_context.messages
        if REMINDER_HEADER in (message.content or "")
    ]


async def _rehydrate(probe: Probe, session: Session) -> None:
    """丢内存态 + 水合（逐出 → 重新订阅）：CM 在**同一磁盘状态**上重建。

    重建路径与"重启后按需恢复"是同一条（`SessionManager` 重新构造 Session /
    ContextManager）——内存里的声明集随之消失，只有 metadata 记录与磁盘链还在。
    probe 侧用显式 release（`/api/session/release`）+ subscribe 驱动——被订阅的
    会话是钉住的，必须先去订阅。
    """
    driver = probe.driver_required
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    released = await session.release()
    assert released == {"ok": True, "released": True, "detail": "released"}, released
    await driver.http.subscribe(session.session_id, driver.client_id)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_hot_switch_freezes_declared_and_reminds_model(probe: Probe) -> None:
    """热切换：声明集冻结 + reminder 注入 + 新工具真的可执行（红线）。

    WHEN 链非空时把工具集从 ``(Bash, Read)`` 扩到 ``(Bash, Read, Write)``
    THEN reminder 立即进链（user 消息）；下一次请求的 ``tools`` 声明仍是**旧集**
    （KV cache 前缀不变），消息里带新增工具名与全量 schema；且新工具真的能执行
    （workspace 文件为证）——第二次请求的声明**依然**是旧集。
    """
    probe.register(
        HOT_MODEL,
        Turn.of(text="reply one"),
        Turn.of(
            tool_calls=[ToolCall("Write", {"path": HOT_FILE, "content": HOT_CONTENT})]
        ),
        Turn.of(text="wrote it"),
    )
    session = await probe.session(model=HOT_MODEL, tools=list(INITIAL_TOOLS))
    await session.chat("alpha")

    baseline = probe.context(HOT_MODEL, 0)
    assert baseline.tool_names == list(INITIAL_TOOLS), baseline.describe()
    assert _reminder_texts(baseline) == [], baseline.describe()

    before_update = probe.history(session)
    await session.set_tools(list(EXTENDED_TOOLS))

    # reminder 在 update 时就进链：已提交历史**只增不改**，链尾多一条 user 角色
    # 的 reminder（下一次请求因此带上它——不需要等一轮）。
    updated = probe.history(session)
    messages = updated.messages()
    assert [message["role"] for message in messages] == ["user", "assistant", "user"]
    reminder_record = messages[-1]
    assert reminder_record["content"].startswith(REMINDER_HEADER), reminder_record
    assert updated.records[: len(before_update.records)] == before_update.records, (
        updated.describe()
    )

    # 下一轮：声明冻结（旧集），reminder 在上下文里；新工具真的能被调用。
    result = await session.chat("beta")
    assert result.data["subtype"] == "success", result.data
    probe.files.assert_content(HOT_FILE, equals=HOT_CONTENT)

    after_switch = probe.context(HOT_MODEL, 1)
    assert after_switch.tool_names == list(INITIAL_TOOLS), after_switch.describe()
    reminder = _reminder_of(after_switch)
    assert "Added: Write (namespace: default)" in reminder, reminder
    assert "Removed:" not in reminder, reminder
    assert "Use ONLY the tools listed in this reminder going forward." in reminder
    assert _reminder_tool_names(reminder) == list(EXTENDED_TOOLS), reminder

    # 冻结跨请求保持：同一轮里的第二次 LLM 调用（工具结果之后）仍是旧集。
    after_tool = probe.context(HOT_MODEL, 2)
    assert after_tool.tool_names == list(INITIAL_TOOLS), after_tool.describe()
    after_tool.assert_tool_pairing()
    assert _reminder_of(after_tool) == reminder


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_cold_switch_updates_declared_without_reminder(probe: Probe) -> None:
    """冷路径（链空）：声明集直接更新，不注入 reminder（红线）。

    WHEN 会话还没有任何 user / assistant 消息时改工具集
    THEN 下一次请求的 ``tools`` 声明就是新集，且消息里**没有** reminder
    （冷路径没有前缀要保护，凭空多一条 reminder 就是回归）。
    """
    probe.register(COLD_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(model=COLD_MODEL, tools=list(EXTENDED_TOOLS))

    response = await session.set_tools(list(INITIAL_TOOLS))
    assert response == {"ok": True}, response
    assert probe.history(session).messages() == [], "冷切换不得往链上写任何消息"

    await session.chat("alpha")

    context = probe.context(COLD_MODEL, 0)
    assert context.tool_names == list(INITIAL_TOOLS), context.describe()
    assert _reminder_texts(context) == [], context.describe()
    assert REMINDER_HEADER not in json.dumps(context.body, ensure_ascii=False)

    # 反向对照：链非空之后再切（热路径）就会注入——证明"没有 reminder"是
    # 冷路径的性质，而不是断言写空了。
    await session.set_tools(list(EXTENDED_TOOLS))
    await session.chat("beta")
    assert _reminder_of(probe.context(COLD_MODEL, 1)).startswith(REMINDER_HEADER)
    assert probe.context(COLD_MODEL, 1).tool_names == list(INITIAL_TOOLS)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_manual_compact_syncs_declared_to_executable(probe: Probe) -> None:
    """compact 打破前缀缓存 → 声明集随可执行集同步（红线）。

    WHEN 热切换（声明冻结在旧集）之后手动 compact
    THEN 压缩调用仍用**冻结的旧集**（与主调用前缀一致，缓存命中最大化）；
    compact 之后的下一次请求声明 = 新集（可执行集），reminder 随被压缩区间
    一起离开上下文。
    """
    probe.register(
        COMPACT_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text=f"<summary>{SUMMARY_TEXT}</summary>"),
        Turn.of(text="post compact reply"),
    )
    session = await probe.session(model=COMPACT_MODEL, tools=list(INITIAL_TOOLS))
    await session.chat("alpha")
    await session.set_tools(list(EXTENDED_TOOLS))
    await session.chat("beta")

    frozen = probe.context(COMPACT_MODEL, 1)
    assert frozen.tool_names == list(INITIAL_TOOLS), frozen.describe()
    assert _reminder_of(frozen).startswith(REMINDER_HEADER)

    response = await session.compact()
    await session.watch.expect("compact_done", within=15)
    assert response == {"ok": True, "original_tokens": 0, "compressed_tokens": 0}, (
        response
    )

    # 压缩调用是非流式的第三次请求：声明仍是冻结的旧集（与主调用前缀一致）。
    compact_call = probe.request(COMPACT_MODEL, 2)
    assert compact_call.stream is False, probe.requests.summary()
    assert compact_call.context().tool_names == list(INITIAL_TOOLS), (
        probe.requests.summary()
    )

    result = await session.chat("gamma")
    assert result.data["result"] == "post compact reply", result.data

    after_compact = probe.context(COMPACT_MODEL, 3)
    assert after_compact.tool_names == list(EXTENDED_TOOLS), after_compact.describe()
    assert _reminder_texts(after_compact) == [], after_compact.describe()
    assert REMINDER_HEADER not in json.dumps(after_compact.body, ensure_ascii=False)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_hot_switch_removal_blocks_execution_and_keeps_declared(
    probe: Probe,
) -> None:
    """热切换的"减"分支：reminder 报移除、可执行集立刻收紧、声明集仍冻结（红线）。

    WHEN 链非空时把 ``(Bash, Read, Write)`` 缩到 ``(Read,)``
    THEN reminder 只有 ``Removed:`` 行（无 ``Added:``）、``<tools>`` 段只剩新集；
    被移除的工具**立刻不可执行**（reminder 承诺的 "you will receive an error"），
    而请求里的 ``tools`` 声明仍是旧的三个——冻结的是声明，不是能力。
    """
    probe.register(
        REMOVAL_MODEL,
        Turn.of(text="reply one"),
        Turn.of(
            tool_calls=[ToolCall("Bash", {"command": f"printf x > {REMOVED_FILE}"})]
        ),
        Turn.of(text="not executed"),
    )
    session = await probe.session(
        model=REMOVAL_MODEL, tools=list(EXTENDED_TOOLS), yolo=True
    )
    await session.chat("alpha")
    assert probe.context(REMOVAL_MODEL, 0).tool_names == list(EXTENDED_TOOLS)

    await session.set_tools(list(REMAINING_TOOLS))

    result = await session.chat("beta")
    assert result.data["subtype"] == "success", result.data

    # 声明集仍冻结在旧集（前缀不变）——新旧差异全在 reminder 里。
    frozen = probe.context(REMOVAL_MODEL, 1)
    assert frozen.tool_names == list(EXTENDED_TOOLS), frozen.describe()
    reminder = _reminder_of(frozen)
    assert "Removed: Bash (namespace: default), Write (namespace: default)" in (
        reminder
    ), reminder
    assert "Added:" not in reminder, reminder
    assert _reminder_tool_names(reminder) == list(REMAINING_TOOLS), reminder

    # 可执行集已经收紧：removed 的工具调用不执行，只回一条错误结果。
    unknown = "Error: unknown tool: Bash, or you don't have permission to use it."
    probe.files.assert_missing(REMOVED_FILE)
    view = probe.history(session)
    tool_messages = [
        message for message in view.messages() if message["role"] == "tool"
    ]
    assert [message["content"] for message in tool_messages] == [unknown], (
        view.describe()
    )
    view.assert_tool_pairing()

    # 冻结跨请求保持：工具结果之后的那次调用仍是旧声明。
    assert probe.context(REMOVAL_MODEL, 2).tool_names == list(EXTENDED_TOOLS)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_declared_set_follows_executable_after_rehydrate(probe: Probe) -> None:
    """重建（逐出 → 水合）：声明集跟随可执行集，不把旧冻结当成新前缀（红线）。

    WHEN 热切换（声明冻结在旧集）之后逐出内存态、再水合重建 CM
    THEN 重建后的请求声明 = 当时的**可执行集**（不是被冻结的旧集、也不重新注入
    reminder——磁盘链上原有的那一条 reminder 照旧在上下文里，但只有一条）。
    """
    probe.register(REBUILD_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(model=REBUILD_MODEL, tools=list(INITIAL_TOOLS))
    await session.chat("alpha")
    await session.set_tools(list(EXTENDED_TOOLS))

    await _rehydrate(probe, session)

    await session.chat("beta")
    context = probe.context(REBUILD_MODEL, 1)
    assert context.tool_names == list(EXTENDED_TOOLS), context.describe()
    # 链上原有的 reminder（热切换那一刻注入的）仍在上下文里——重建不重放也不
    # 重复注入第二条。
    reminders = _reminder_texts(context)
    assert len(reminders) == 1, context.describe()
    assert "Added: Write (namespace: default)" in reminders[0], reminders[0]
    assert probe.history(session).messages()[-1]["content"] == "reply two"
