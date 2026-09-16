"""fork 红线场景（tasks 6.13–6.15，spec probe-scenarios「Requirement: fork 红线场景」）。

覆盖的断言点：

- ``test_fork_chain_integrity_and_uuid_remap``：分叉链完整性与 uuid 重映射——子链 =
  源链前缀（不含目标）、uuid 全量重映射且与源无交集、事件随行、不变量通过；
- ``test_fork_metadata_snapshot``：分叉 metadata 快照——``metadata.json`` 含
  ``forked_from`` / ``workspace`` / ``template_name`` / 模型与 provider 快照；
- ``test_fork_isolation_and_evolution_equivalence``：隔离与演进等价（双写对账）——
  双方各自请求的上下文与各自链一致（子侧首个请求 == fork 点的语义前缀）；一侧的
  继续不影响另一侧的链与请求。

metadata 断言口径（实施期事实口径 3）：子侧记录的是 **fork 时刻的快照**，源侧之后
的变更会漂移，因此这里直接断言子侧值 == 场景预期（模型 / workspace / 模板）与源
session id（``forked_from``），而不是"子侧 == 源侧当前值"。
"""

from __future__ import annotations

import json
from datetime import datetime

import pytest

from wing_probe import (
    FORK_METADATA_FIELDS,
    HistoryView,
    Probe,
    Session,
    Turn,
    assert_fork_of,
    fork_metadata,
    node_semantics,
)

INTEGRITY_MODEL = "probe/fork-integrity"
METADATA_MODEL = "probe/fork-metadata"
ISOLATION_MODEL = "probe/fork-isolation"


async def _two_turns(probe: Probe, model: str) -> Session:
    """两轮对话（alpha / beta），返回会话句柄——fork 素材。"""
    probe.register(
        model,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text="reply three"),
        Turn.of(text="reply four"),
    )
    session = await probe.session(model=model)
    await session.chat("alpha")
    await session.chat("beta")
    return session


def _fingerprint(view: HistoryView) -> list[str]:
    """全量记录指纹（隔离断言用：逐条记录逐字段对比）。"""
    return [
        json.dumps(record, ensure_ascii=False, sort_keys=True)
        for record in view.records
    ]


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_fork_chain_integrity_and_uuid_remap(probe: Probe) -> None:
    """分叉链完整性与 uuid 重映射（spec: 分叉链完整性与 uuid 重映射）。

    WHEN 在中间消息处 fork
    THEN 断言通过：子链 = 源链前缀（不含目标）；uuid 全量重映射且与源无交集；
    事件随行；不变量自动检查通过。
    """
    session = await _two_turns(probe, INTEGRITY_MODEL)
    source = probe.history(session)
    source_chain_uuids = [record["uuid"] for record in source.active_chain()]
    target = source.messages()[2]  # 第二轮用户消息 "beta"
    prefix_end = source_chain_uuids.index(target["uuid"])

    child = await session.fork(target["uuid"])
    child_view = probe.history(child)

    fork_call = probe.last_http_call(path="/api/session/fork")
    assert fork_call is not None and fork_call.status == 200, fork_call
    assert child.session_id in [item.session_id for item in probe.sessions]
    material = assert_fork_of(source, child_view, target["uuid"])
    assert child.response["draft"] == material["expected_draft"] == "beta"

    # 子链 == 源链中目标之前的完整前缀（含事件节点，共 N 个）。
    assert material["prefix_length"] == prefix_end == len(material["uuid_map"])
    assert material["chain_length"] == prefix_end, child_view.describe()
    assert [record["uuid"] for record in child_view.active_chain()] == [
        material["uuid_map"][uuid] for uuid in source_chain_uuids[:prefix_end]
    ], child_view.describe()
    assert child_view.tip_uuid == child_view.last_record_uuid

    # uuid 全量重映射：与源记录集零交集。
    assert not (set(child_view.by_uuid) & set(source.by_uuid)), sorted(
        set(child_view.by_uuid) & set(source.by_uuid)
    )

    # 事件随行：前缀窗口内的事件逐条等价（按 type + 顺序）。
    source_events = [
        record
        for record in source.active_chain()[:prefix_end]
        if record["role"] == "event"
    ]
    child_events = [
        record for record in child_view.active_chain() if record["role"] == "event"
    ]
    assert [record["type"] for record in child_events] == [
        record["type"] for record in source_events
    ], child_view.describe()
    assert child_events, "前缀里应当有事实事件（用户消息确认 / turn 边界）"
    for expected, actual in zip(source_events, child_events):
        assert node_semantics(expected) == node_semantics(actual), (expected, actual)

    # 目标及其后续不随 fork 走。
    assert target["uuid"] not in child_view.by_uuid
    assert "gamma" not in [message["content"] for message in child_view.messages()]
    child_view.assert_chain_invariants()
    child_view.assert_tool_pairing()
    child_view.assert_no_transient_records()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_fork_metadata_snapshot(probe: Probe) -> None:
    """分叉 metadata 快照（spec: 分叉 metadata 快照）。

    WHEN fork 出新会话
    THEN 断言通过：``metadata.json`` 含 ``forked_from``（源 id）/ ``workspace`` /
    ``template_name`` / 模型与 provider 快照。
    """
    session = await _two_turns(probe, METADATA_MODEL)
    source = probe.history(session)
    target = source.messages()[2]

    child = await session.fork(target["uuid"])

    metadata_path = child.session_dir / "metadata.json"
    assert metadata_path.is_file(), sorted(
        item.name for item in child.session_dir.iterdir()
    )
    metadata = fork_metadata(child.session_dir, require=FORK_METADATA_FIELDS)

    assert metadata["forked_from"] == session.session_id
    assert metadata["workspace"] == str(session.workspace)
    assert metadata["template_name"] == "default"
    assert metadata["model_name"] == METADATA_MODEL
    assert metadata["provider_name"] == "probe"
    assert datetime.fromisoformat(metadata["last_interaction"]), metadata

    # metadata 是"fork 时刻一次写全"的快照：子侧不依赖源侧后续状态。
    assert set(FORK_METADATA_FIELDS) <= set(metadata), sorted(metadata)
    assert json.loads(metadata_path.read_text(encoding="utf-8")) == metadata

    # 链侧同样成立（metadata 与链是同一次 fork 的产物）。
    child_view = probe.history(child)
    material = assert_fork_of(source, child_view, target["uuid"])
    assert material["child_metadata"] == metadata
    assert child_view.metadata() == metadata


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_fork_isolation_and_evolution_equivalence(probe: Probe) -> None:
    """隔离与演进等价（双写对账）（spec: 隔离与演进等价（双写对账））。

    WHEN fork 后源与子各继续一轮对话
    THEN 断言通过：双方各自请求的上下文与各自链一致（子侧首个请求 == fork 点的
    语义前缀）；一侧的继续不影响另一侧的链与请求。
    """
    session = await _two_turns(probe, ISOLATION_MODEL)
    source = probe.history(session)
    target = source.messages()[2]  # 用户消息 "beta"

    child = await session.fork(target["uuid"])
    child_at_fork = probe.history(child)
    source_at_fork = probe.history(session)

    # 源先继续一轮……
    assert (await session.chat("src-next")).data["result"] == "reply three"
    source_after = probe.history(session)
    child_untouched = probe.history(child)

    # ……子侧的链与请求完全没被源侧演进影响。
    assert _fingerprint(child_untouched) == _fingerprint(child_at_fork), (
        "源侧继续对话改动了子侧链"
    )
    requests = probe.requests_for(ISOLATION_MODEL)
    assert len(requests) == 3, probe.requests.summary()
    assert all("child-next" not in json.dumps(item.body) for item in requests)

    # 子侧再继续一轮……
    assert (await child.chat("child-next")).data["result"] == "reply four"
    child_after = probe.history(child)
    source_untouched = probe.history(session)

    # ……源侧的链与请求完全没被子侧演进影响。
    assert _fingerprint(source_untouched) == _fingerprint(source_after), (
        "子侧继续对话改动了源侧链"
    )

    # 双写对账：子侧首个请求 == fork 点的语义前缀 + 子侧新消息。
    requests = probe.requests_for(ISOLATION_MODEL)
    assert len(requests) == 4, probe.requests.summary()
    source_request = next(
        item for item in requests if "src-next" in json.dumps(item.body)
    )
    child_request = next(
        item for item in requests if "child-next" in json.dumps(item.body)
    )
    source_context = source_request.context()
    child_context = child_request.context()

    child_context.assert_prefix_like(
        ["user: alpha", "assistant: reply one", "user: child-next"]
    )
    assert child_context.role_sequence() == ["user", "assistant", "user"], (
        child_context.role_sequence()
    )
    source_context.assert_prefix_like(
        [
            "user: alpha",
            "assistant: reply one",
            "user: beta",
            "assistant: reply two",
            "user: src-next",
        ]
    )

    # 两侧请求的前缀（fork 点）语义等价——uuid 不同、上下文一致。
    shared = min(len(child_context.messages), len(source_context.messages)) - 1
    assert shared == 2, (child_context.describe(), source_context.describe())
    for position in range(shared):
        left = child_context.messages[position]
        right = source_context.messages[position]
        assert (left.role, left.content) == (right.role, right.content), (
            position,
            left.summary(),
            right.summary(),
        )
    assert not (set(child_at_fork.by_uuid) & set(source_at_fork.by_uuid)), (
        "fork 之后两侧记录集必须 uuid 不相交"
    )

    # 各自链与各自请求一致（最后一次请求的尾部 == 该链的 Message 投影）。
    assert [message["content"] for message in child_after.messages()] == [
        "alpha",
        "reply one",
        "child-next",
        "reply four",
    ], child_after.describe()
    assert [message["content"] for message in source_after.messages()] == [
        "alpha",
        "reply one",
        "beta",
        "reply two",
        "src-next",
        "reply three",
    ], source_after.describe()
    assert child_context.texts("user") == ["alpha", "child-next"], (
        child_context.describe()
    )
    assert source_context.texts("user") == ["alpha", "beta", "src-next"], (
        source_context.describe()
    )
    assert "reply two" not in json.dumps(child_context.body), child_context.describe()
    assert "child-next" not in json.dumps(source_context.body), (
        source_context.describe()
    )
