"""compact 红线场景（tasks 6.5–6.8，spec probe-scenarios「Requirement: compact 红线场景」）。

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

import json

import pytest

from wing_probe import (
    HistoryView,
    Probe,
    Turn,
    assert_compact_transition,
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

#: 结构锚点：格式约束段的起点（``<summary>`` 标签契约，spec 也在用它）。
FORMAT_ANCHOR = "Wrap your summary in <summary></summary> tags."
#: 结构锚点：格式约束的末位 recency 段。
CRITICAL_ANCHOR = "## CRITICAL"


def _fingerprint(view: HistoryView) -> list[str]:
    """全量记录指纹（零变更红线用：逐条记录逐字段对比）。"""
    return [
        json.dumps(record, ensure_ascii=False, sort_keys=True)
        for record in view.records
    ]


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
