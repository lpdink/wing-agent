"""中途加入的状态快照：`SyncSession.status` 是权威状态，不是内容推断。

中途订阅者听不到已经过去的 `TurnStarted`（一次性 live 事件，`persist=false`
不落盘、不重放），所以它"在不在 working"只能由快照回答。快照曾把状态**隐含**
在内容里——前端用 `uncommitted` / `uncommitted_tools` 是否为空反推 working。
这个推断只在"一轮 LLM 调用已经吐出过已终结块"时成立，漏掉两个相位：

- **首帧未到**：turn 已 working，但第一轮 LLM 调用还在飞行（TTFT、首块到达前的
  网络/排队、`get_messages_for_llm` 里 await 后台 compact）——内容投影全空；
- **轮边界**：上一轮 `add_messages` 提交后 accumulator 置空，下一轮首个块到达前
  ——每个多轮 turn 的每一次工具往返都要穿过它。

两个相位都是"模型调用在飞行"，也正是用户最可能去 resume/join 的时刻（列表里
那个 session 正显示 working）。本场景把四个相位的快照状态**钉死**：

- ``test_midjoin_before_first_chunk_reports_working``：相位 A——内容投影为空，
  `status` 仍必须是 ``working``（回归点）；
- ``test_midjoin_while_tool_running_reports_working``：工具执行中——assistant 轮
  已终结未提交，`uncommitted` 非空且 `status``working``；
- ``test_midjoin_pending_ask_reports_waiting``：挂起的 ask——`status` 是
  ``waiting``（turn 仍在飞行，只是阻塞在用户输入上），且 ask 仍可答；
- ``test_midjoin_idle_session_reports_idle``：无进行中 turn——``idle`` 且无
  `turn_started_at`。
"""

from __future__ import annotations

from collections.abc import AsyncIterator
from contextlib import asynccontextmanager

import pytest

from wing_probe import Driver, Probe, ToolCall, Turn
from wing_probe.watch import Event

#: 场景私有 model 名（design D3：剧本按 model 名路由，场景之间不共享）。
FIRST_CHUNK_MODEL = "probe/midjoin-first-chunk"
TOOL_MODEL = "probe/midjoin-tool"
ASK_MODEL = "probe/midjoin-ask"
IDLE_MODEL = "probe/midjoin-idle"


@asynccontextmanager
async def midjoin(probe: Probe, session_id: str) -> AsyncIterator[Event]:
    """第二个 client 中途订阅，产出它收到的 ``sync_session`` 事件。

    独立于场景 driver 的新连接（新 ``client_id``）——这才是"中途加入"的形态：
    subscribe 把快照推给新 client，而它错过了此前的全部 live 事件。
    """
    other = await Driver.connect(probe.env)
    try:
        joined = await other.attach(session_id)
        yield await joined.watch.expect("sync_session", within=5.0)
    finally:
        await other.close()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_midjoin_before_first_chunk_reports_working(probe: Probe) -> None:
    """首个内容块到达前 join：快照必须是 working（回归点）。

    WHEN turn 已开始、剧本首个内容块尚未吐出（``delay`` 撑开的窗口）
    AND 新 client 恰好在此窗口订阅
    THEN 快照 ``status == "working"``——尽管 ``uncommitted`` / ``uncommitted_tools``
    都是空的（内容为空 ≠ 不在跑）；``turn_started_at`` 供前端恢复已耗时。

    delay 取 3s 而不是刚够用：这个相位没有别的东西撑着窗口（工具轮的窗口是
    工具执行时长），"确实落在首帧之前"这条证据必须离机器繁忙时的假红足够远。
    用 ``first_delay`` 而不是 ``delay``：撑开窗口的是**首个内容帧之前**的等待，
    ``delay`` 是逐帧的（5 帧 × 3s = 12s），那是白付的。
    """
    probe.register(FIRST_CHUNK_MODEL, Turn.of(text="late answer", first_delay=3.0))
    session = await probe.session(model=FIRST_CHUNK_MODEL)

    await session.send("hi")
    await session.watch.expect("turn_started", within=5.0)

    async with midjoin(probe, session.session_id) as sync:
        assert sync.data["status"] == "working", sync.data
        # 相位证据：内容投影两项皆空（wire_dump 剥 null → 键缺失）。
        assert "uncommitted" not in sync.data, sync.data
        assert sync.data["uncommitted_tools"] == [], sync.data
        assert sync.data["turn_started_at"], sync.data
        sync_at = sync.at

    # 确实落在"首个内容块之前"的窗口里：文本流在快照之后才到达。
    text = await session.watch.expect("text", within=15.0)
    assert text.at > sync_at, (sync_at, text.at)

    result = await session.watch.expect("turn_result", within=20.0)
    assert result.data["subtype"] == "success", result.data


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_midjoin_while_tool_running_reports_working(probe: Probe) -> None:
    """工具执行中 join：快照 working，且未提交投影已带出工具卡。

    WHEN 剧本发起一次 ``sleep`` 的 Bash 调用、工具尚在执行
    AND 新 client 在此窗口订阅
    THEN ``status == "working"`` 且 ``uncommitted`` 是带 tool_calls 的 assistant
    投影（已终结未提交块）——内容与状态两个事实同时为真。
    """
    probe.register(
        TOOL_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "sleep 1"})]),
        Turn.of(text="done"),
    )
    session = await probe.session(model=TOOL_MODEL, yolo=True)

    await session.send("run it")
    await session.watch.expect("tool_call", within=10.0)

    async with midjoin(probe, session.session_id) as sync:
        assert sync.data["status"] == "working", sync.data
        uncommitted = sync.data.get("uncommitted")
        assert uncommitted and uncommitted["tool_calls"], sync.data

    result = await session.watch.expect("turn_result", within=20.0)
    assert result.data["subtype"] == "success", result.data


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_midjoin_pending_ask_reports_waiting(probe: Probe) -> None:
    """挂起 ask 期间 join：快照 waiting，且该 ask 仍可答。

    WHEN 剧本调用 AskUserQuestion，用户尚未答复
    AND 新 client 在此窗口订阅
    THEN ``status == "waiting"``（turn 仍在飞行，阻塞在用户输入上），下发的
    事实事件里带仍挂起的 ask——中途订阅者能直接回答它。
    """
    probe.register(
        ASK_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall(
                    "AskUserQuestion",
                    {
                        "questions": [
                            {
                                "id": "flavor",
                                "header": "Flavor",
                                "question": "Which flavor?",
                                "options": [{"label": "vanilla"}, {"label": "mocha"}],
                            }
                        ]
                    },
                )
            ]
        ),
        Turn.of(text="thanks"),
    )
    session = await probe.session(model=ASK_MODEL)

    await session.send("pick something")
    ask = await session.watch.expect("ask", within=15)

    async with midjoin(probe, session.session_id) as sync:
        assert sync.data["status"] == "waiting", sync.data
        replayed = [e for e in sync.data["events"] if e.get("type") == "ask"]
        assert replayed and replayed[0]["tool_call_id"] == ask.data["tool_call_id"], (
            sync.data["events"]
        )

    await session.answer(ask, "flavor: vanilla")
    result = await session.watch.expect("turn_result", within=15.0)
    assert result.data["subtype"] == "success", result.data


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_midjoin_idle_session_reports_idle(probe: Probe) -> None:
    """无进行中 turn：快照 idle（不能把"没有内容"读成"在跑"的反面）。

    WHEN 会话跑完一轮、回到 idle
    AND 新 client 订阅
    THEN ``status == "idle"``、无 ``turn_started_at``、无未提交投影。
    """
    probe.register(IDLE_MODEL, Turn.of(text="ok"))
    session = await probe.session(model=IDLE_MODEL)
    await session.chat("hi")

    async with midjoin(probe, session.session_id) as sync:
        assert sync.data["status"] == "idle", sync.data
        assert "uncommitted" not in sync.data, sync.data
        assert sync.data["uncommitted_tools"] == [], sync.data
        assert "turn_started_at" not in sync.data, sync.data
