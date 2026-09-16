"""rewind 红线场景（tasks 6.9–6.12，spec probe-scenarios「Requirement: rewind 红线场景」）。

覆盖的断言点：

- ``test_middle_rewind_chain_shape_and_draft``：中间回退——返回 draft 等于目标消息
  内容；活跃链末端为复制行（等价于目标 parent、uuid 全新、parent 为祖父）；
  目标及其后续不在活跃链、仍在记录集；
- ``test_rewind_then_continue_chain_order``：回退后再发送——新消息链序接在复制行
  之后（下轮请求上下文与"分叉点"语义一致）；不变量自动检查通过；
- ``test_rewind_skips_event_ancestors``：回退边界跳过事件节点——目标消息的直接
  parent 是事件记录时，复制源取最近 Message 而非事件节点；
- ``test_rewind_to_root``：回退到根——对首条用户消息回退得到 ``[rewind_to_root]``
  语义（形态以实跑为准并在场景中记录）；后续可继续对话且不变量通过。
"""

from __future__ import annotations

import json

import pytest

from wing_probe import (
    Probe,
    Session,
    Turn,
    Usage,
    assert_rewind_transition,
    is_event,
)

MIDDLE_MODEL = "probe/rewind-middle"
CONTINUE_MODEL = "probe/rewind-continue"
EVENT_PARENT_MODEL = "probe/rewind-event-parent"
ROOT_MODEL = "probe/rewind-root"


async def _three_turns(probe: Probe, model: str) -> Session:
    """三轮对话（alpha / beta / gamma），返回会话句柄。

    第 1 轮带 ``usage``（假 Provider 是 usage 的驱动源）——落盘的 assistant 行
    因此携带 provider 审计字段，rewind 的"复制行不复制 usage / stop_reason"
    断言才有实证对象（见 :func:`test_middle_rewind_chain_shape_and_draft`）。
    """
    probe.register(
        model,
        Turn.of(
            text="reply one",
            usage=Usage(prompt_tokens=1234, completion_tokens=56, cached_tokens=7),
        ),
        Turn.of(text="reply two"),
        Turn.of(text="reply three"),
        Turn.of(text="reply four"),
    )
    session = await probe.session(model=model)
    for text in ("alpha", "beta", "gamma"):
        await session.chat(text)
    return session


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_middle_rewind_chain_shape_and_draft(probe: Probe) -> None:
    """中间回退的链形状与 draft（spec: 中间回退的链形状与 draft）。

    WHEN 对一张多轮历史的中间消息执行 rewind
    THEN 断言通过：返回 draft 等于目标消息内容；活跃链末端为复制行（等价于目标
    parent、uuid 全新、parent 为祖父）；目标及其后续不在活跃链、仍在记录集。
    """
    session = await _three_turns(probe, MIDDLE_MODEL)
    before = probe.history(session)
    messages = before.messages()
    assert [message["content"] for message in messages] == [
        "alpha",
        "reply one",
        "beta",
        "reply two",
        "gamma",
        "reply three",
    ], before.describe()

    target = messages[2]  # 中间的用户消息 "beta"
    source = messages[1]  # 目标的最近 Message 祖先 "reply one"
    before_chain_uuids = [record["uuid"] for record in before.active_chain()]

    response = await session.rewind(target["uuid"])
    after = probe.history(session)

    assert response["ok"] is True, response
    material = assert_rewind_transition(before, after, target["uuid"])
    assert response["draft"] == material["expected_draft"] == "beta"
    assert material["mode"] == "copy", material
    assert material["source_uuid"] == source["uuid"], material
    rewind_call = probe.last_http_call(path="/api/session/rewind")
    assert rewind_call is not None and rewind_call.status == 200, rewind_call

    # 活跃链末端 = 复制行（tip 即复制行；rewind 是"移动 tip + 追加复制行"）。
    chain = after.active_chain()
    copy = chain[-1]
    assert copy["uuid"] == material["copy_uuid"] == after.tip_uuid
    assert copy["uuid"] not in before.by_uuid, "复制行必须是全新 uuid"
    assert copy["role"] == source["role"] == "assistant", copy
    assert copy["content"] == source["content"] == "reply one", copy
    assert copy["parent_uuid"] == source["parent_uuid"], copy  # 祖父
    assert copy["parent_uuid"] == messages[0]["uuid"], copy

    # 复制行**不复制** provider 审计字段（usage / stop_reason）——实现的上下文
    # 事实口径（`HistoryView.message_semantics` 的比较口径同源）。先确认源行
    # 真的带 usage（否则这条断言是空转），再断言复制行没有它。
    assert "usage" in source and source["usage"]["prompt_tokens"] == 1234, source
    assert "usage" not in copy, copy
    assert "stop_reason" not in copy, copy
    assert set(copy) <= set(source) - {"usage", "stop_reason"}, sorted(copy)

    # 目标及其后续被移出活跃链（但仍留在记录集里）。
    after_chain_uuids = [record["uuid"] for record in chain]
    for dropped in before_chain_uuids[before_chain_uuids.index(target["uuid"]) :]:
        assert dropped not in after_chain_uuids, dropped
        assert dropped in after.by_uuid, dropped
    assert (
        material["dropped_uuids"]
        == before_chain_uuids[before_chain_uuids.index(target["uuid"]) :]
    ), material

    # 复制行之前的前缀链 = 原链中祖父（含）之前的部分；也就是说复制行**取代**
    # 了源行在活跃链上的位置（源行仍在记录集里，但不再是 tip 的祖先）。
    prefix_end = before_chain_uuids.index(source["uuid"])
    assert [record["uuid"] for record in chain[:-1]] == before_chain_uuids[
        :prefix_end
    ], after.describe()
    assert source["uuid"] in after.by_uuid, "源行必须留在记录集里"
    assert source["uuid"] not in after_chain_uuids, after.describe()
    after.assert_chain_invariants()
    assert [message["content"] for message in after.messages()] == [
        "alpha",
        "reply one",
    ], after.describe()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_rewind_then_continue_chain_order(probe: Probe) -> None:
    """回退后继续发送的链序（spec: 回退后继续发送的链序）。

    WHEN rewind 后发送一条新消息并完成一轮
    THEN 断言通过：新消息链序接在复制行之后（下轮请求上下文与"分叉点"语义一致）；
    不变量自动检查通过。
    """
    session = await _three_turns(probe, CONTINUE_MODEL)
    before = probe.history(session)
    target = before.messages()[2]  # 用户消息 "beta"

    await session.rewind(target["uuid"])
    rewind_view = probe.history(session)
    material = assert_rewind_transition(before, rewind_view, target["uuid"])

    result = await session.chat("beta2")
    assert result.data["result"] == "reply four", result.data
    after = probe.history(session)

    # 链序：复制行 → （新轮的 user_message_accepted / turn_started 事件）→ 新消息
    # → 新回复；新消息是复制行的后代。
    new_user = after.messages()[2]
    assert new_user["content"] == "beta2", after.describe()
    assert new_user["uuid"] != target["uuid"], "新消息必须是新 uuid"
    from_copy = after.chain_ending_at(new_user["uuid"])
    from_copy_uuids = [record["uuid"] for record in from_copy]
    assert material["copy_uuid"] in from_copy_uuids, after.describe()
    assert from_copy_uuids[-1] == new_user["uuid"]
    copy_position = from_copy_uuids.index(material["copy_uuid"])
    assert all(is_event(record) for record in from_copy[copy_position + 1 : -1]), (
        after.describe()
    )

    assert [message["content"] for message in after.messages()] == [
        "alpha",
        "reply one",
        "beta2",
        "reply four",
    ], after.describe()
    assert after.messages()[1]["uuid"] == material["copy_uuid"]
    assert after.tip_uuid == after.last_record_uuid

    # 下轮请求上下文 = 分叉点语义前缀 + 新消息（旧分支的 beta / reply two / gamma /
    # reply three 一律不出现）。
    assert probe.requests.count(CONTINUE_MODEL) == 4, probe.requests.summary()
    context = probe.context(CONTINUE_MODEL, 3)
    context.assert_prefix_like(["user: alpha", "assistant: reply one", "user: beta2"])
    assert context.role_sequence() == ["user", "assistant", "user"], (
        context.role_sequence()
    )
    body = json.dumps(context.body, ensure_ascii=False)
    for abandoned in ("reply two", "gamma", "reply three"):
        assert abandoned not in body, (abandoned, context.describe())


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_rewind_skips_event_ancestors(probe: Probe) -> None:
    """回退边界跳过事件节点（spec: 回退边界跳过事件节点）。

    WHEN 目标消息的直接 parent 是事件记录（如 diff / ask）
    THEN 断言通过：复制源取到最近 Message 而非事件节点；链形状断言通过。

    构造：第二轮的用户消息 parent 是 ``turn_started`` / ``user_message_accepted`` /
    ``turn_result`` / ``done`` 这类事实事件节点链，最近 Message 祖先才是上一条
    assistant 回复——这正是"跳过事件节点回溯"的边界。
    """
    probe.register(
        EVENT_PARENT_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
    )
    session = await probe.session(model=EVENT_PARENT_MODEL)
    await session.chat("alpha")
    await session.chat("beta")

    before = probe.history(session)
    target = before.messages()[2]  # 第二轮用户消息 "beta"
    parent = before.by_uuid[target["parent_uuid"]]
    assert is_event(parent), before.describe()
    assert parent["type"] == "turn_started", parent

    response = await session.rewind(target["uuid"])
    after = probe.history(session)
    material = assert_rewind_transition(before, after, target["uuid"])

    assert response["draft"] == "beta"
    assert material["mode"] == "copy", material
    assert material["skipped_event_uuids"], (
        "目标上方应当有事件节点被跳过：" + before.describe()
    )
    assert parent["uuid"] in material["skipped_event_uuids"], material
    for skipped in material["skipped_event_uuids"]:
        assert is_event(before.by_uuid[skipped]), skipped

    # 复制源 = 最近的 Message 祖先（而不是被跳过的事件节点）。
    source = before.messages()[1]
    assert material["source_uuid"] == source["uuid"], material
    copy = after.active_chain()[-1]
    assert copy["role"] == source["role"] == "assistant", copy
    assert copy["content"] == source["content"] == "reply one", copy
    assert copy["uuid"] != parent["uuid"], "复制行不得复用事件节点身份"
    assert copy["parent_uuid"] == before.messages()[0]["uuid"], copy
    assert [message["content"] for message in after.messages()] == [
        "alpha",
        "reply one",
    ], after.describe()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_rewind_to_root(probe: Probe) -> None:
    """回退到根（spec: 回退到根）。

    WHEN 对首条用户消息执行 rewind
    THEN 断言通过：活跃链回到根语义（``[rewind_to_root]`` 或等价行为，形态以实跑
    为准并在场景中记录）；后续可继续对话且不变量通过。

    实跑形态（记录）：首条用户消息的最近 Message 祖先不存在（其上方只有
    ``user_message_accepted`` 事件），实现写入哨兵节点 ``role="system"`` /
    ``content="[rewind_to_root]"`` / 无 parent；活跃链消息层面只剩这一个节点。
    """
    probe.register(
        ROOT_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text="reply three"),
    )
    session = await probe.session(model=ROOT_MODEL)
    await session.chat("alpha")
    await session.chat("beta")

    before = probe.history(session)
    target = before.messages()[0]  # 首条用户消息 "alpha"

    response = await session.rewind(target["uuid"])
    after = probe.history(session)
    material = assert_rewind_transition(before, after, target["uuid"])

    assert response["draft"] == material["expected_draft"] == "alpha"
    assert material["mode"] == "root", material
    assert material["source_uuid"] is None, material
    assert material["copy_uuid"] == after.tip_uuid

    chain = after.active_chain()
    assert len(chain) == 1, after.describe()
    sentinel = chain[0]
    assert sentinel["role"] == "system", sentinel
    assert sentinel["content"] == "[rewind_to_root]", sentinel
    assert not sentinel.get("parent_uuid"), sentinel
    assert after.messages() == chain

    # 后续可继续对话：新消息接在根语义节点之后。
    result = await session.chat("fresh start")
    assert result.data["result"] == "reply three", result.data
    continued = probe.history(session)
    assert [message["content"] for message in continued.messages()] == [
        "[rewind_to_root]",
        "fresh start",
        "reply three",
    ], continued.describe()
    assert continued.messages()[0]["uuid"] == sentinel["uuid"]
    continued.assert_chain_invariants()

    # 下轮请求：旧对话（alpha / beta / reply one / reply two）全部不在上下文里。
    assert probe.requests.count(ROOT_MODEL) == 3, probe.requests.summary()
    context = probe.context(ROOT_MODEL, 2)
    body = json.dumps(context.body, ensure_ascii=False)
    for abandoned in ("alpha", "beta", "reply one", "reply two"):
        assert abandoned not in body, (abandoned, context.describe())
    assert context.messages[-1].content == "fresh start", context.describe()
