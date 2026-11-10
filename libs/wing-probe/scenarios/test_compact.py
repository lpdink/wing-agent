"""compact 红线场景。

覆盖的断言点：

- ``test_manual_compact_chain_shape``：手动压缩链形状——活跃链（消息层面）= 单条
  摘要节点（``[Compact]`` 前缀、``unzip_last_uuid`` == 操作前链末消息 uuid）、
  允许其后跟 ``compact_done`` 事件、旧节点全在、链拓扑不变量通过；
- ``test_request_after_compact``：压缩后下轮请求构成——``system + 摘要``（被压缩
  区间不出现）、KV cache 前缀语义（前缀 = system + 摘要）；
- ``test_compact_instruction_rendering``：压缩指令条件渲染——带指令时指令文本位于
  摘要主体之后、格式约束（CRITICAL 段）之前；不带指令时 prompt 与默认策略形态一致
  （结构化对账，不逐字节复刻文案）；
- ``test_compact_failure_leaves_no_trace``：压缩失败不半提交（红线）——操作失败
  （错误可观测）且活跃链与记录集零变更。

事实口径（2026-09-16 实施期确认，见 spec 场景内引用）：**手动** compact 把操作前
整个上下文窗口压成单条摘要节点（无保留区）；**后台**自动压缩才是"摘要节点 +
重链接保留区"。本文件断言手动路径。
"""

from __future__ import annotations

import asyncio
import json
import time
from collections.abc import Callable

import pytest

from wing_probe import (
    PENDING_KEY,
    HistoryView,
    message_semantics,
    Probe,
    Turn,
    Usage,
    assert_compact_transition,
    wait_pending,
)
from wing_probe.driver import DriverHttpError

#: 摘要产物（假 Provider 的压缩调用返回带 ``<summary>`` 的文本）。
SUMMARY_TEXT = "Task: answer the user. State: two turns done. Next: continue."
COMPACT_CONTENT = f"[Compact] {SUMMARY_TEXT}"

SHAPE_MODEL = "probe/compact-shape"
REQUEST_MODEL = "probe/compact-request"
INSTRUCTION_MODEL = "probe/compact-with-instruction"
PLAIN_MODEL = "probe/compact-without-instruction"
FAILURE_MODEL = "probe/compact-failure"

#: 结构锚点：格式约束段的起点（``<summary>`` 标签契约，格式约束段也依赖它）。
FORMAT_ANCHOR = "Wrap your summary in <summary></summary> tags."
#: 结构锚点：格式约束的末位 recency 段。
CRITICAL_ANCHOR = "## CRITICAL"


def _fingerprint(view: HistoryView) -> list[str]:
    """全量记录指纹（零变更红线用：逐条记录逐字段对比）。"""
    return [
        json.dumps(record, ensure_ascii=False, sort_keys=True)
        for record in view.records
    ]


@pytest.mark.probe_env(models=[SHAPE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_manual_compact_chain_shape(probe: Probe) -> None:
    """手动压缩的链形状（spec: 手动压缩的链形状）。

    WHEN 多轮历史后执行手动 compact（假 Provider 返回带 ``<summary>`` 的压缩产物）
    THEN 断言通过：活跃链（消息层面）= 单条摘要节点（``content`` 以 ``[Compact]``
    前缀、``unzip_last_uuid`` == 操作前链末消息 uuid）；允许其后跟事件节点
    （``compact_done``）；操作前全部节点仍存在于记录集；链拓扑不变量通过。
    """
    probe.register(
        SHAPE_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text=f"<summary>{SUMMARY_TEXT}</summary>"),
    )
    session = await probe.session(model=SHAPE_MODEL)
    await session.chat("alpha")
    await session.chat("beta")

    before = probe.history(session)
    last_message_uuid = before.messages()[-1]["uuid"]

    response = await session.compact()
    await session.watch.expect("compact_done", within=15)
    after = probe.history(session)

    assert response == {"ok": True, "original_tokens": 0, "compressed_tokens": 0}, (
        response
    )
    material = assert_compact_transition(before, after)

    # 整窗压缩：压缩区 = 操作前活跃链上的**全部** Message，保留区为空。
    assert material["compressed_messages"] == len(before.messages()), material
    assert material["unzip_last_uuid"] == last_message_uuid, material
    assert material["tail_source_uuids"] == [], material
    assert material["after_chain_uuids"][0] == material["compact_uuid"], material

    # 消息层面新链只有摘要节点；其后允许跟（且只跟）事件节点。
    messages = after.messages()
    assert len(messages) == 1, after.describe()
    compact_node = messages[0]
    assert compact_node["uuid"] == material["compact_uuid"]
    assert compact_node["role"] == "assistant", compact_node
    assert compact_node["content"] == COMPACT_CONTENT, compact_node
    assert compact_node["content"].startswith("[Compact]"), compact_node
    assert "parent_uuid" not in compact_node or not compact_node["parent_uuid"]
    assert material["trailing_uuids"] == [after.active_chain()[-1]["uuid"]], material
    assert [event["type"] for event in after.events()] == ["compact_done"], (
        after.describe()
    )

    # 旧节点不删（compact 是 append-only）。
    assert set(before.by_uuid) <= set(after.by_uuid), sorted(
        set(before.by_uuid) - set(after.by_uuid)
    )
    assert after.tip_uuid == after.last_record_uuid
    after.assert_chain_invariants()
    after.assert_no_transient_records()

    # 链上没有任何内容节点复用旧 uuid（新链 = 全新摘要节点 + 事件）。
    assert material["compact_uuid"] not in before.by_uuid

    # 现场转储在场景里同样可用：artifacts 含链副本 + 压缩请求留档 + 事件时间线。
    artifacts = await probe.dump()
    assert artifacts == probe.artifacts_path
    copied = artifacts / "sessions" / session.session_id / "history.jsonl"
    assert copied.is_file(), sorted(path.name for path in artifacts.iterdir())
    assert copied.read_text(encoding="utf-8") == after.path.read_text(encoding="utf-8")
    requests = json.loads((artifacts / "requests.json").read_text(encoding="utf-8"))
    assert [entry["stream"] for entry in requests] == [True, True, False], requests
    assert requests[-1]["body"]["messages"][-1]["role"] == "user", requests[-1]["body"]
    timeline = (artifacts / "timeline.jsonl").read_text(encoding="utf-8")
    assert '"type": "compact_done"' in timeline, timeline[-400:]


@pytest.mark.probe_env(models=[REQUEST_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_request_after_compact(probe: Probe) -> None:
    """压缩后的下轮请求构成（spec: 压缩后的下轮请求构成）。

    WHEN compact 后继续发送一轮
    THEN 断言通过：该轮请求的上下文 = ``system + 摘要``（被压缩区间不出现）；
    KV cache 语义（前缀 = system + 摘要）被断言。
    """
    probe.register(
        REQUEST_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text=f"<summary>{SUMMARY_TEXT}</summary>"),
        Turn.of(text="post compact reply"),
    )
    session = await probe.session(model=REQUEST_MODEL)
    await session.chat("alpha")
    await session.chat("beta")
    await session.compact()
    await session.watch.expect("compact_done", within=15)

    after = probe.history(session)
    compact_node = after.messages()[0]

    result = await session.chat("gamma")
    assert result.data["result"] == "post compact reply", result.data

    assert probe.requests.count(REQUEST_MODEL) == 4, probe.requests.summary()
    context = probe.context(REQUEST_MODEL, 3)

    # KV cache 前缀语义：请求前缀恰好是 system + 摘要节点，随后才是新消息。
    assert context.all_messages[0].role == "system", context.describe()
    assert context.system == probe.env.system_prompt, context.describe()
    assert context.messages[0].content == compact_node["content"] == COMPACT_CONTENT
    assert context.messages[0].role == "assistant", context.describe()
    context.assert_prefix_like(
        [
            {"role": "assistant", "content": COMPACT_CONTENT},
            "user: gamma",
        ]
    )
    assert context.role_sequence() == ["assistant", "user"], context.role_sequence()
    assert len(context.all_messages) == 3, context.describe()

    # 被压缩区间（旧消息 + 旧回复）在其后的请求里彻底不出现。
    body = json.dumps(context.body, ensure_ascii=False)
    for compressed in ("alpha", "beta", "reply one", "reply two"):
        assert compressed not in body, (compressed, context.describe())


@pytest.mark.probe_env(models=[INSTRUCTION_MODEL, PLAIN_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_compact_instruction_rendering(probe: Probe) -> None:
    """压缩指令的条件渲染（spec: 压缩指令的条件渲染）。

    WHEN 分别以带 instruction 与不带 instruction 执行 compact
    THEN 断言通过：带指令时压缩请求中出现指令文本且位于摘要主体之后、格式约束
    （CRITICAL 段）之前；不带指令时压缩请求与默认策略形态一致（结构化断言，
    不逐字节复刻文案）。
    """
    instruction = "keep the architecture decisions and open questions"
    probe.register(
        INSTRUCTION_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text=f"<summary>{SUMMARY_TEXT}</summary>"),
    )
    probe.register(
        PLAIN_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text=f"<summary>{SUMMARY_TEXT}</summary>"),
    )

    instructed = await probe.session(model=INSTRUCTION_MODEL)
    plain = await probe.session(model=PLAIN_MODEL)
    for session in (instructed, plain):
        await session.chat("alpha")

    await instructed.compact(instruction=instruction)
    await instructed.watch.expect("compact_done", within=15)
    await plain.compact()
    await plain.watch.expect("compact_done", within=15)

    # 压缩调用（非流式）是各 session 的第二次请求，prompt 追加为最后一条 user 消息。
    instructed_prompt = probe.context(INSTRUCTION_MODEL, 1).messages[-1].content
    plain_prompt = probe.context(PLAIN_MODEL, 1).messages[-1].content
    assert probe.request(INSTRUCTION_MODEL, 1).stream is False
    assert probe.request(PLAIN_MODEL, 1).stream is False

    body_prefix = plain_prompt[: plain_prompt.index(FORMAT_ANCHOR)]
    format_suffix = plain_prompt[plain_prompt.index(FORMAT_ANCHOR) :]
    assert FORMAT_ANCHOR in plain_prompt and CRITICAL_ANCHOR in plain_prompt

    # 不带指令：与默认策略形态一致——没有指令块，格式约束段直接跟在主体之后。
    assert "Additional instruction" not in plain_prompt, plain_prompt
    assert instructed_prompt.startswith(body_prefix), instructed_prompt[:400]
    assert instructed_prompt.endswith(format_suffix), instructed_prompt[-400:]

    # 带指令：指令块插在主体与格式约束之间（摘要主体之后、CRITICAL 段之前）。
    inserted = instructed_prompt[
        len(body_prefix) : len(instructed_prompt) - len(format_suffix)
    ]
    assert instruction in inserted, inserted
    assert "Additional instruction" in inserted, inserted
    assert CRITICAL_ANCHOR not in inserted, inserted
    assert plain_prompt != instructed_prompt, "指令必须真的改变 prompt"


@pytest.mark.probe_env(models=[FAILURE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_compact_failure_leaves_no_trace(probe: Probe) -> None:
    """压缩失败不半提交（红线）（spec: 压缩失败不半提交（红线））。

    WHEN 假 Provider 对压缩调用返回不含 ``<summary>`` 标签的文本
    THEN 断言通过：操作失败（错误可观测）且**活跃链与记录集零变更**
    （对比操作前后 ``HistoryView`` 全等）；不变量自动检查通过。
    """
    probe.register(
        FAILURE_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="I forgot the summary tags entirely"),
        Turn.of(text=f"<summary>{SUMMARY_TEXT}</summary>"),
    )
    session = await probe.session(model=FAILURE_MODEL)
    await session.chat("alpha")

    before = probe.history(session)

    with pytest.raises(DriverHttpError) as failure:
        await session.compact()

    status = failure.value.status
    assert 400 <= status < 600, failure.value
    assert "summary" in json.dumps(failure.value.call.response).lower(), failure.value

    after = probe.history(session)

    # 红线：零变更——记录集、活跃链、tip、消息/事件分类全部逐字相同。
    assert _fingerprint(after) == _fingerprint(before), (
        f"records changed after a failed compact\n"
        f"before: {before.describe()}\nafter: {after.describe()}"
    )
    assert [record["uuid"] for record in after.active_chain()] == [
        record["uuid"] for record in before.active_chain()
    ]
    assert after.tip_uuid == before.tip_uuid
    assert len(after.records) == len(before.records)

    # 失败没有留下任何"已完成压缩"的痕迹（也不得落链）。
    session.watch.assert_never("compact_done")
    assert "Compact" not in json.dumps(after.records, ensure_ascii=False)
    after.assert_chain_invariants()
    after.assert_tool_pairing()

    # 失败之后同一个 session 仍然可用：重试一次合法压缩成功，且压缩区末端
    # 仍是失败前的链末消息（失败路径不留任何半提交残留）。
    retry = await session.compact()
    await session.watch.expect("compact_done", within=15)
    recovered = probe.history(session)
    recovered_material = assert_compact_transition(after, recovered)
    assert retry == {"ok": True, "original_tokens": 0, "compressed_tokens": 0}, retry
    assert recovered_material["unzip_last_uuid"] == after.messages()[-1]["uuid"]
    assert recovered_material["compressed_messages"] == len(after.messages())
    assert [event["type"] for event in recovered.events()] == ["compact_done"]


# ===================================================================
# 手动压缩的准入 / 互斥 / 可中断（会话级独占操作）
# ===================================================================
#
# 判据（`agent/core.py::WingAgent.compact` + `agent/inbox.py` 的取出口闸门）：
# 非 idle / 有排队输入 / 已有压缩在飞一律 409 拒绝；压缩期间到达的消息留在 inbox，
# 压缩结束才开始处理；interrupt 取消在飞压缩且链零变更。

BUSY_MODEL = "probe/compact-busy"
DROPS_PENDING_MODEL = "probe/compact-drops-pending"
QUEUES_INPUT_MODEL = "probe/compact-queues-input"
CANCELLED_MODEL = "probe/compact-cancelled"

AUTO_SUMMARY = "Task: answer the user. State: two turns done. Next: continue."
AUTO_SUMMARY_LOOKING = f"<summary>{AUTO_SUMMARY}</summary>"
MANUAL_SUMMARY = "manual summary body"


async def _poll(condition: Callable[[], bool], *, within: float = 10.0) -> None:
    """轮询到条件成立（假 Provider 的请求留档 / 事件游标之外的确定信号）。"""
    expires = time.monotonic() + within
    while not condition():
        if time.monotonic() >= expires:
            raise AssertionError(f"condition not met within {within}s")
        await asyncio.sleep(0.02)


def _compact_calls(probe: Probe, model: str) -> list:
    return [request for request in probe.requests_for(model) if not request.stream]


def _message_fingerprint(view: HistoryView) -> list[str]:
    """消息层的逐字段指纹（事件节点不算——interrupt 等动作会合法地追加事件）。"""
    return [
        json.dumps(message_semantics(message), sort_keys=True)
        for message in view.messages()
    ]


@pytest.mark.probe_env(models=[BUSY_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_manual_compact_refused_while_working(probe: Probe) -> None:
    """turn 在飞时手动压缩被拒（409，链零变更）。

    WHEN 一轮 LLM 调用还在跑（``first_delay`` 撑住首个内容帧）时 POST compact
    THEN 409「会话不空闲」；这一轮照常收口，活跃链与记录集零变更、没有
    ``compact_done``。用户必须先 ``interrupt`` 再压缩。
    """
    probe.register(
        BUSY_MODEL,
        Turn.of(text="slow reply", first_delay=1.5),
        Turn.of(text=f"<summary>{MANUAL_SUMMARY}</summary>"),
    )
    session = await probe.session(model=BUSY_MODEL)
    await session.send("alpha")
    await session.watch.expect("turn_started", within=15)

    with pytest.raises(DriverHttpError) as refused:
        await session.compact()
    assert refused.value.status == 409, refused.value
    assert "idle" in str(refused.value), refused.value

    # 这一轮照常收口；压缩既没发生也没留下痕迹。
    await session.watch.expect("turn_result", within=20)
    session.watch.assert_never("compact_done")
    after = probe.history(session)
    assert after.messages()[-1]["role"] == "assistant", after.describe()
    assert all("[Compact]" not in (m["content"] or "") for m in after.messages())

    # 空闲之后同一个会话照常可以手动压缩。
    await session.compact()
    await session.watch.expect("compact_done", within=15)
    assert probe.history(session).messages()[0]["content"] == (
        f"[Compact] {MANUAL_SUMMARY}"
    )


@pytest.mark.probe_env(
    models=[DROPS_PENDING_MODEL],
    context_window_tokens=1000,
    keep_recent_tokens=400,
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_manual_compact_discards_pending_auto_compact(probe: Probe) -> None:
    """手动压缩丢弃待生效的自动压缩（不和它抢链）。

    WHEN 后台压缩已落盘 pending（aux 在）时执行手动压缩
    THEN 手动压缩成功；aux 被清掉；活跃链是手动摘要（自动压缩的产物永不换入），
    下一轮请求里也看不到自动压缩的摘要文本。
    """
    usage = (300, 900, 1200)
    probe.register(
        DROPS_PENDING_MODEL,
        Turn.of(text="reply one", usage=Usage(prompt_tokens=usage[0])),
        Turn.of(text="reply two", usage=Usage(prompt_tokens=usage[1])),
        Turn.of(text=AUTO_SUMMARY_LOOKING, usage=Usage(prompt_tokens=usage[2])),
        Turn.of(text=AUTO_SUMMARY_LOOKING, usage=Usage(prompt_tokens=usage[2])),
        Turn.of(text=f"<summary>{MANUAL_SUMMARY}</summary>"),
        Turn.of(text="post manual reply", usage=Usage(prompt_tokens=1500)),
    )
    session = await probe.session(model=DROPS_PENDING_MODEL)
    await session.chat("alpha")
    await session.chat("beta")
    await session.chat("gamma")
    pending = await wait_pending(probe, session)
    assert pending["compact_content"].startswith("[Compact] Task:"), pending

    response = await session.compact()
    assert response["ok"] is True, response
    await session.watch.expect("compact_done", within=15)

    after = probe.history(session)
    assert after.aux(PENDING_KEY) is None, after.describe()
    assert [message["content"] for message in after.messages()] == [
        f"[Compact] {MANUAL_SUMMARY}"
    ], after.describe()

    # 下一轮：请求前缀 = system + 手动摘要；自动压缩的摘要文本一处都看不到。
    result = await session.chat("delta")
    assert result.data["result"] == "post manual reply", result.data
    body = json.dumps(probe.context(DROPS_PENDING_MODEL, 5).body, ensure_ascii=False)
    assert MANUAL_SUMMARY in body, body[:400]
    assert AUTO_SUMMARY not in body, body[:400]


@pytest.mark.probe_env(models=[QUEUES_INPUT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_message_during_manual_compact_waits(probe: Probe) -> None:
    """压缩期间投递的消息不驱动 turn：压缩结束才开始处理。

    WHEN 手动压缩在飞（``first_delay`` 撑住压缩调用）时发一条用户消息
    THEN 压缩收口（``compact_done``）之前不出现 ``turn_started``；压缩结束后这一轮
    才开跑，且它的请求里已经是压缩后的上下文（system + 摘要 + 新消息）。
    """
    probe.register(
        QUEUES_INPUT_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text=f"<summary>{MANUAL_SUMMARY}</summary>", first_delay=1.5),
        Turn.of(text="reply two"),
    )
    session = await probe.session(model=QUEUES_INPUT_MODEL)
    await session.chat("alpha")

    cursor = session.watch.cursor
    compacting = asyncio.create_task(session.compact())
    await _poll(lambda: bool(_compact_calls(probe, QUEUES_INPUT_MODEL)))

    await session.send("queued during compaction")
    # 压缩在飞：这一轮不得开始。
    await session.watch.expect_none("turn_started", within=0.4)

    await compacting
    await session.watch.expect("turn_result", within=20)
    session.watch.assert_ordered(["compact_done", "turn_started"], since=cursor)

    context = probe.context(QUEUES_INPUT_MODEL, 2)
    assert context.messages[0].content == f"[Compact] {MANUAL_SUMMARY}", (
        context.describe()
    )
    assert context.messages[-1].content == "queued during compaction", (
        context.describe()
    )


@pytest.mark.probe_env(models=[CANCELLED_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_interrupt_cancels_manual_compact(probe: Probe) -> None:
    """interrupt 取消在飞的手动压缩：请求以 409 收尾，链零变更。

    WHEN 手动压缩在飞时 ``interrupt``
    THEN 压缩请求返回 409「compaction interrupted」、没有 ``compact_done``、
    活跃链与记录集零变更（压缩只在最后一个 await 之后写链）；此后会话仍可用
    ——下一条消息的请求里既没有摘要也没有残留的半截状态。
    """
    probe.register(
        CANCELLED_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text=f"<summary>{MANUAL_SUMMARY}</summary>", first_delay=3.0),
        Turn.of(text="reply two"),
    )
    session = await probe.session(model=CANCELLED_MODEL)
    await session.chat("alpha")
    before = probe.history(session)

    compacting = asyncio.create_task(session.compact())
    await _poll(lambda: bool(_compact_calls(probe, CANCELLED_MODEL)))
    await session.interrupt()

    with pytest.raises(DriverHttpError) as cancelled:
        await compacting
    assert cancelled.value.status == 409, cancelled.value
    assert "interrupt" in str(cancelled.value).lower(), cancelled.value

    session.watch.assert_never("compact_done")
    after = probe.history(session)
    # 消息层零变更——interrupt 自己会落一条 interrupted 事实事件，那是它的正常产物。
    assert _message_fingerprint(after) == _message_fingerprint(before), after.describe()
    after.assert_chain_invariants()

    # 取消之后照常继续：请求前缀 = system + 原链（摘要不在）。
    result = await session.chat("beta")
    assert result.data["result"] == "reply two", result.data
    context = probe.context(CANCELLED_MODEL, 2)
    assert context.messages[0].content == "alpha", context.describe()
    assert MANUAL_SUMMARY not in json.dumps(context.body, ensure_ascii=False)
