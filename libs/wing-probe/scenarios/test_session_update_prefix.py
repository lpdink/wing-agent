"""`POST /api/session/update` 的 title / workspace：显示面与执行面分离。

两个字段都经同一条端点写入，但契约完全不同，重构 `Session.update_state` 时最容易
串线：

- **title 只进 metadata 与列表显示名**（`session/list` 的 `name`、`session/get`、
  `metadata.json.session_name`）——不进入 LLM 请求体、不注入 System Reminder、
  不改变上下文前缀（KV cache 无关）；
- **workspace 只影响执行面**（Bash 的 cwd、工具相对路径的解析基准）——同样**不**
  进入请求体；改完后 Bash / Write 必须落在新目录。

覆盖的断言点：

- ``test_title_change_is_display_only``：改标题 → 三处显示面一致；下一轮请求的
  整段 body 里搜不到标题字符串、消息序列没有多出任何东西；与改标题前的请求
  前缀逐项一致（system / tools / 开关 / 旧消息）；
- ``test_workspace_change_moves_bash_and_file_tools``：ws-a 建会话写 `rel-a.txt`
  → 改到 ws-b → Bash `pwd`/相对写、Write 相对路径都落在 ws-b；请求前缀逐项不变
  （workspace 不进请求体）；非法 workspace 被拒（400）且 live 值不被部分应用。

> "请求前缀不变"是把**实际观察到的**前缀影响写成断言（影响为零）：workspace /
> title 都不参与请求组装，重构时若有人把它们塞进 system 段或追加 System Reminder，
> 这里的逐字节对账会直接红。
"""

from __future__ import annotations

import json

import pytest

from wing_probe import DriverHttpError, LoggedRequest, Probe, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
TITLE_MODEL = "probe/update-title"
WORKSPACE_MODEL = "probe/update-workspace"

#: 唯一化标题（在请求体 JSON 里做全文搜索，必须不可能被其他字段撞上）。
PROBE_TITLE = "PROBE-TITLE-7a3f-display-only"

#: 请求体里影响服务端处理的开关（前缀身份的一部分）。
FLAG_KEYS = ("enable_thinking", "preserve_thinking", "reasoning_effort")


def _flags(body: dict) -> dict:
    return {key: body.get(key) for key in FLAG_KEYS}


def _assert_prefix_identity(
    before: LoggedRequest, after: LoggedRequest, *, shared: int
) -> None:
    """共享前缀逐项对账：system / tools 声明 / 处理开关 / 前 ``shared`` 条消息。"""
    assert after.body["messages"][0] == before.body["messages"][0], (
        "system 段必须逐字节一致"
    )
    assert after.body["tools"] == before.body["tools"], "tools 声明必须一致"
    assert _flags(after.body) == _flags(before.body), "处理开关必须一致"
    for position in range(shared):
        left = before.context().messages[position]
        right = after.context().messages[position]
        assert (left.role, left.content) == (right.role, right.content), (
            position,
            left.summary(),
            right.summary(),
        )


async def _sessions(probe: Probe) -> list[dict]:
    payload = await probe.driver_required.http.list_sessions()
    return list(payload.get("sessions", []))


async def _list_entry(probe: Probe, session_id: str) -> dict:
    for entry in await _sessions(probe):
        if entry.get("id") == session_id:
            return entry
    raise AssertionError(f"session {session_id} not in /api/session/list")


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_title_change_is_display_only(probe: Probe) -> None:
    """标题只进显示面：列表/详情/metadata 一致，请求体里搜不到、前缀不变。"""
    probe.register(TITLE_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(model=TITLE_MODEL)
    await session.chat("alpha")

    # 自动标题 = 首条消息（改标题前的基线）。
    assert (await _list_entry(probe, session.session_id))["name"] == "alpha"

    await probe.driver_required.http.update_session(
        session.session_id, title=PROBE_TITLE
    )

    # 显示面三处一致：会话列表、会话详情、落盘 metadata。
    assert (await _list_entry(probe, session.session_id))["name"] == PROBE_TITLE
    state = await session.get()
    assert state["name"] == PROBE_TITLE, state
    metadata = probe.history(session).metadata() or {}
    assert metadata.get("session_name") == PROBE_TITLE, metadata

    # 请求面：标题字符串在整段请求体里不存在（唯一的出现地被钉死在此）。
    await session.chat("beta")
    after = probe.request(TITLE_MODEL, 1)
    assert PROBE_TITLE not in json.dumps(after.body, ensure_ascii=False), (
        "title 不得进入 LLM 请求体",
        after.body,
    )

    # 消息序列没有被注入额外内容（例如 System Reminder）。
    messages = after.context().messages
    assert [message.role for message in messages] == ["user", "assistant", "user"], [
        message.summary() for message in messages
    ]
    assert messages[-1].content == "beta", messages[-1].summary()
    # 落盘链同样只有对话消息（事件 / reminder 不应因此出现）。
    assert [message["role"] for message in probe.history(session).messages()] == [
        "user",
        "assistant",
        "user",
        "assistant",
    ]

    # 前缀身份：与改标题前的请求逐项一致。
    before = probe.request(TITLE_MODEL, 0)
    _assert_prefix_identity(before, after, shared=len(before.context().messages))


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_workspace_change_moves_bash_and_file_tools(probe: Probe) -> None:
    """workspace 变更：Bash cwd 与工具相对路径跟着走；请求前缀不变；非法值 400。"""
    ws_a = probe.env.root / "ws-a"
    ws_b = probe.env.root / "ws-b"
    ws_b.mkdir(parents=True, exist_ok=True)
    probe.register(
        WORKSPACE_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "printf one > rel-a.txt"})]),
        Turn.of(text="file a written"),
        Turn.of(
            tool_calls=[ToolCall("Bash", {"command": "pwd && printf two > rel-b.txt"})]
        ),
        Turn.of(text="file b written"),
        Turn.of(
            tool_calls=[ToolCall("Write", {"path": "rel-c.txt", "content": "three"})]
        ),
        Turn.of(text="file c written"),
    )
    session = await probe.session(model=WORKSPACE_MODEL, workspace=ws_a, yolo=True)

    first = await session.chat("write file a")
    assert first.data["subtype"] == "success", first.data
    probe.files_of(session).assert_content("rel-a.txt", equals="one")

    # 切换 workspace（必须在磁盘上存在）。
    await probe.driver_required.http.update_session(
        session.session_id, workspace=str(ws_b)
    )
    info = await session.info()
    assert info["workdir"] == str(ws_b.resolve()), info
    assert (await _list_entry(probe, session.session_id))["workspace"] == str(
        ws_b.resolve()
    )
    metadata = probe.history(session).metadata() or {}
    assert metadata.get("workspace") == str(ws_b.resolve()), metadata

    # 执行面：Bash 的 cwd 与相对路径基准都换成 ws-b（rel-b.txt 只应出现在那里）。
    second = await session.chat("write file b")
    assert second.data["subtype"] == "success", second.data
    tool_results = session.watch.events("tool_call_result")
    pwd_result = next(
        event for event in tool_results if "pwd" in json.dumps(event.data["tool_args"])
    )
    assert str(ws_b.resolve()) in pwd_result.data["tool_result"], pwd_result.data
    # 句柄的 workspace 停在创建时的 ws-a（`files_of(句柄)` 用它）；改 workspace
    # 后事实在 metadata 里——按 session id 取视图（回读 metadata.json.workspace）。
    probe.files_of(session.session_id).assert_content("rel-b.txt", equals="two")
    assert not (ws_a / "rel-b.txt").exists(), "Bash 不得仍在旧 workspace 里执行"

    # 文件工具（Write）的相对路径同样按 ctx.cwd 解析。
    third = await session.chat("write file c")
    assert third.data["subtype"] == "success", third.data
    probe.files_of(session.session_id).assert_content("rel-c.txt", equals="three")
    assert not (ws_a / "rel-c.txt").exists()

    # 请求面：workspace 不进请求体——第三轮请求（每轮两个 LLM 请求：工具轮 +
    # 收尾轮）与第一轮请求共享完整前缀，且消息序列恰好是两轮对话 + 本轮入站
    # （没有 System Reminder 之类的插入）。
    before = probe.request(WORKSPACE_MODEL, 0)
    after = probe.request(WORKSPACE_MODEL, 4)
    assert after.context().messages[-1].content == "write file c", after.describe()
    _assert_prefix_identity(before, after, shared=len(before.context().messages))
    assert [message.role for message in after.context().messages] == [
        "user",
        "assistant",
        "tool",
        "assistant",
        "user",
        "assistant",
        "tool",
        "assistant",
        "user",
    ], [message.summary() for message in after.context().messages]

    # 非法 workspace：400，且 live 值保持 ws-b（拒绝是原子的，不部分应用）。
    with pytest.raises(DriverHttpError) as failure:
        await probe.driver_required.http.update_session(
            session.session_id, workspace=str(probe.env.root / "does-not-exist")
        )
    assert failure.value.status == 400, failure.value.call.render()
    assert (await session.info())["workdir"] == str(ws_b.resolve())
