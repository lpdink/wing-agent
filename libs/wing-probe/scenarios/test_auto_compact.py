"""自动（early）压缩 + pending compact 跨重启恢复红线场景（spec「自动压缩」）。

被守的语义（`libs/core/wing/context/manager.py` + `context/compaction.py`）：

- **双阈值**：`compact_window = context_window_tokens - keep_recent_tokens`
  （early trigger）与 `context_window_tokens`（apply）。请求组装时先看 pending：
  够 apply 就换入（压缩节点 + 重链接 tail），否则看后台 task，再不济才触发新的
  early compact；
- **early compact 是后台 task**：结果先落 **aux kv 通道**
  （`<session_dir>/pending_compact.json`，key = `pending_compact`），**下一轮请求**
  才换入消息链——"已生成未生效"的状态只在盘上，不在链上；
- **压缩区尾部保留**：后台压缩把"最后一条 user 消息之前的 head"按 token 切割成
  压缩区，`start_uuid` / `end_uuid` 是被压缩区两端——apply 时按 uuid 在当前链上
  自校验（找不到就丢弃）；
- **跨进程重启恢复**：pending 在盘上（aux），新进程重建 CM 时读回（进程重启时
  内存里的 task / result 全没了）——下一轮请求照样 apply。
- **relink 只换链坐标**：apply 重链接保留区（tail）时整条复制消息——上下文事实
  与 provider 审计字段（`stop_reason` / `usage`）、媒体引用都随行（手抄字段清单
  会随 schema 漂移，静默丢字段）。

确定性来源（不是等待运气）：`@pytest.mark.probe_env` 把上下文窗口压到千级，
剧本用 `usage.prompt_tokens` 精确驱动阈值；"待生效状态已落盘"用**轮询到 aux
出现**判定（后台 task 的完成时刻不可预测），之后的一切都是确定性的。
"""

from __future__ import annotations

import asyncio
import json
import time

import pytest

from wing_probe import (
    LoggedRequest,
    Probe,
    Session,
    Turn,
    Usage,
    assert_compact_transition,
    message_semantics,
)
from wing_probe.history.view import COMPACT_PREFIX, HistoryView

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
EARLY_MODEL = "probe/auto-compact-early"
RESTART_MODEL = "probe/auto-compact-restart"

#: 上下文窗口旋钮（agent 配置）：compact_window = 1000 - 400 = 600 tokens。
#: 首轮无服务端 usage 时走本地估算（system + 一条 user ≈ 46 tokens）→ 不触发；
#: 剧本回报的 1100 跨过 600（early）与 1000（apply）两个阈值。
CONTEXT_WINDOW = 1000
KEEP_RECENT = 400
COMPACT_WINDOW = CONTEXT_WINDOW - KEEP_RECENT
SERVER_TOKENS = 1100

#: 压缩产物（假 Provider 的压缩调用返回带 ``<summary>`` 的文本）。
SUMMARY_TEXT = "Task: answer the user. State: two turns done. Next: continue."
COMPACT_CONTENT = f"{COMPACT_PREFIX} {SUMMARY_TEXT}"

#: aux kv 通道的 key（``ContextManager._PENDING_COMPACT_AUX_KEY``）。
PENDING_KEY = "pending_compact"

#: 轮询预算：后台压缩是本地一次 HTTP 往返，给足余量即失败可读。
PENDING_DEADLINE = 20.0
POLL_INTERVAL = 0.05

#: 主调用与后台压缩**共用**的回复文本：压缩请求与主调用是并发发出的两条
#: 请求（谁先到假 Provider 由事件循环调度决定），两轮的剧本因此写成同文本，
#: 让结论与到达顺序无关——这不是"断言宽松"，是消除一个与红线无关的竞态。
SUMMARY_LOOKING_TEXT = f"<summary>{SUMMARY_TEXT}</summary>"

#: 压缩 prompt 的固定锚点（``Compactor._COMPACT_PROMPT_FORMAT``）。
FORMAT_ANCHOR = "Wrap your summary in <summary></summary> tags."

#: 手动压缩才有的侧重指令块锚点（后台路径必须没有它）。
INSTRUCTION_ANCHOR = "Additional instruction from the user"


def _script() -> tuple[Turn, ...]:
    """剧本：首轮真实文本 + 其余同文本（主调用 / 后台压缩共用）。

    第 4 次请求（``gamma``）一定是第 4 个 Turn（见测试内的推演：``gamma`` 只有在
    pending 落盘之后才发出，此时后台压缩早已消费过一个 Turn），因此给它一段
    **可辨识**的文本，用来钉死"apply 之后的那一轮"。
    """
    usage = Usage(prompt_tokens=SERVER_TOKENS)
    return (
        Turn.of(text="reply one", usage=usage),
        Turn.of(text=SUMMARY_LOOKING_TEXT, usage=usage),
        Turn.of(text=SUMMARY_LOOKING_TEXT, usage=usage),
        Turn.of(text="post compact reply", usage=usage),
    )


def _streamed(probe: Probe, model: str) -> list[LoggedRequest]:
    """某 model 的**主调用**留档（非流式的只有后台 / 手动压缩）。"""
    return [request for request in probe.requests_for(model) if request.stream]


def _compact_calls(probe: Probe, model: str) -> list[LoggedRequest]:
    """某 model 的非流式留档（压缩调用）。"""
    return [request for request in probe.requests_for(model) if not request.stream]


async def _wait_pending(
    probe: Probe, session: Session, *, deadline: float = PENDING_DEADLINE
) -> dict:
    """轮询到 pending compact 已落盘（返回 aux 内容）。

    失败报告带上请求留档：超时意味着"后台 task 没跑成 / 没落盘"，
    留档能立刻区分"压缩请求压根没发出"与"发出但结果没落盘"。
    """
    observed: dict | None = None
    expires = time.monotonic() + deadline
    while time.monotonic() < expires:
        observed = probe.history(session).aux(PENDING_KEY)
        if observed is not None:
            return observed
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"no pending compact under key {PENDING_KEY!r} within {deadline:.0f}s "
        f"(aux path: {probe.history(session).aux_path(PENDING_KEY)}); "
        f"requests: {probe.requests.summary()}"
    )


def _assert_early_compact_shape(probe: Probe, model: str, pending: dict) -> None:
    """后台压缩请求的形状 + 待生效状态的形状（不依赖主调用的到达顺序）。"""
    calls = _compact_calls(probe, model)
    assert len(calls) == 1, probe.requests.summary()
    body = json.dumps(calls[0].body, ensure_ascii=False)
    assert FORMAT_ANCHOR in body, body[:400]
    assert INSTRUCTION_ANCHOR not in body, body[:400]

    assert pending["compact_content"] == COMPACT_CONTENT, pending
    assert isinstance(pending["start_uuid"], str) and pending["start_uuid"], pending
    assert isinstance(pending["end_uuid"], str) and pending["end_uuid"], pending


def _assert_applied(
    probe: Probe,
    session: Session,
    before: HistoryView,
    *,
    model: str,
) -> None:
    """apply 之后的三个面：请求体 / 落盘链 / aux 清空。"""
    after = probe.history(session)
    material = assert_compact_transition(before, after, allow_trailing=True)

    # 落盘链：压缩节点（unzip 指向被压缩区末端）+ 重链接 tail（uuid 全新）。
    assert material["compact_uuid"] is not None
    compact_node = after.by_uuid[material["compact_uuid"]]
    assert compact_node["content"] == COMPACT_CONTENT, compact_node
    assert compact_node["unzip_last_uuid"] == material["unzip_last_uuid"]
    assert material["compressed_messages"] == 2, material

    # 下一轮请求：前缀 = system + 压缩节点，被压缩区间逐字消失。
    context = _streamed(probe, model)[-1].context()
    assert context.all_messages[0].role == "system", context.describe()
    assert context.system == probe.env.system_prompt, context.describe()
    assert context.messages[0].content == COMPACT_CONTENT, context.describe()
    assert context.messages[0].role == "assistant", context.describe()
    body = json.dumps(context.body, ensure_ascii=False)
    for compressed in ("alpha", "reply one"):
        assert compressed not in body, (compressed, context.describe())

    # 待生效状态已消费：aux 清空（下一轮不会再 apply 一次）。
    assert probe.history(session).aux(PENDING_KEY) is None
    after.assert_chain_invariants()
    after.assert_tool_pairing()


@pytest.mark.probe_env(
    models=[EARLY_MODEL],
    context_window_tokens=CONTEXT_WINDOW,
    keep_recent_tokens=KEEP_RECENT,
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_early_compact_persists_then_applies(probe: Probe) -> None:
    """early trigger → pending 落盘（链未变）→ 下一轮 apply（红线）。

    WHEN 上下文跨过 ``compact_window``（服务端 usage 1100 ≥ 600）
    THEN 后台压缩产出 pending（aux 落盘、**活跃链不变**：还没生效）、无
    ``compact_done``（不是手动路径）；下一轮请求把 pending 换入——请求前缀变成
    ``system + 压缩节点``，被压缩区间（首轮 user / assistant）逐字消失，
    落盘链是「压缩节点 + 重链接 tail」，aux 被清空。
    """
    probe.register(EARLY_MODEL, *_script())
    session = await probe.session(model=EARLY_MODEL)

    # 阈值前提：剧本回报的 1100 同时跨过 early（600 = 1000 − 400）与 apply（1000）
    # 两个阈值——这是本场景能触发后台压缩、并在下一轮换入的全部算术（首轮没有
    # 服务端 usage，走本地估算 ≈46，远低于 early）。
    assert COMPACT_WINDOW <= SERVER_TOKENS and SERVER_TOKENS >= CONTEXT_WINDOW

    # 首轮：无服务端 usage，本地估算远低于阈值——不触发任何压缩。
    await session.chat("alpha")
    assert len(probe.requests_for(EARLY_MODEL)) == 1, probe.requests.summary()
    snapshot = probe.history(session)
    alpha_uuid, reply_one_uuid = [message["uuid"] for message in snapshot.messages()]

    # 第二轮：服务端 usage 跨过 early 阈值 → 后台压缩（主调用照常跑完）。
    await session.chat("beta")
    pending = await _wait_pending(probe, session)
    _assert_early_compact_shape(probe, EARLY_MODEL, pending)

    # pending 的内容 = 被压缩区间两端（最后一条 user 消息之前的部分）——尚未生效。
    assert pending["start_uuid"] == alpha_uuid, pending
    assert pending["end_uuid"] == reply_one_uuid, pending
    unapplied = probe.history(session)
    assert [message["content"] for message in unapplied.messages()] == [
        "alpha",
        "reply one",
        "beta",
        SUMMARY_LOOKING_TEXT,
    ], unapplied.describe()
    session.watch.assert_never("compact_done")
    assert probe.last_http_call(path="/api/session/compact") is None

    # 第三轮：pending 换入 → 请求前缀与落盘链同步改变。
    before_apply = probe.history(session)
    result = await session.chat("gamma")
    assert result.data["result"] == "post compact reply", result.data
    _assert_applied(probe, session, before_apply, model=EARLY_MODEL)

    # 压缩区末端（被压缩区）仍在记录里（compact 是 append-only），只是不在链上。
    final = probe.history(session)
    assert final.by_uuid[reply_one_uuid]["content"] == "reply one", final.describe()
    assert [message["content"] for message in final.messages()] == [
        COMPACT_CONTENT,
        "beta",
        SUMMARY_LOOKING_TEXT,
        "gamma",
        "post compact reply",
    ], final.describe()


@pytest.mark.probe_env(
    models=[RESTART_MODEL],
    context_window_tokens=CONTEXT_WINDOW,
    keep_recent_tokens=KEEP_RECENT,
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_pending_compact_survives_process_restart(probe: Probe) -> None:
    """pending compact 跨**进程重启**恢复（红线）。

    WHEN pending 已落盘后杀掉网关进程、在同一 ``WING_HOME`` / sessions 上重启
    THEN aux 仍在（待生效状态在盘上，不在内存里）；``resume`` 在水合时读回它，
    下一轮请求照样 apply（压缩节点进前缀、被压缩区间消失、aux 清空）。
    """
    probe.register(RESTART_MODEL, *_script())
    session = await probe.session(model=RESTART_MODEL)

    await session.chat("alpha")
    await session.chat("beta")
    pending = await _wait_pending(probe, session)
    _assert_early_compact_shape(probe, RESTART_MODEL, pending)

    before_apply = probe.history(session)
    old_port = probe.env.port
    old_client = probe.driver_required.client_id

    await probe.restart_gateway()

    # 进程换了：端口 / client_id 都变了，盘上的待生效状态一字不动。
    assert probe.env.port != old_port, (old_port, probe.env.port)
    assert probe.driver_required.client_id != old_client
    resurrected = probe.history(session)
    assert resurrected.aux(PENDING_KEY) == pending, resurrected.describe()
    assert [record["uuid"] for record in resurrected.active_chain()] == [
        record["uuid"] for record in before_apply.active_chain()
    ], resurrected.describe()

    # 新进程里按需水合（重建 CM），下一轮 apply 生效。
    resumed = await probe.resume(session.session_id)
    assert resumed.session_id == session.session_id
    result = await resumed.chat("gamma")
    assert result.data["result"] == "post compact reply", result.data

    _assert_applied(probe, resumed, before_apply, model=RESTART_MODEL)

    # 留档跨重启连续（假 Provider 不重启）：4 次请求 = 首轮 + beta 轮 + 压缩 +
    # 重启后的 gamma 轮，压缩请求在它之前。
    assert len(probe.requests_for(RESTART_MODEL)) == 4, probe.requests.summary()
    assert (
        _compact_calls(probe, RESTART_MODEL)[0].index
        < _streamed(probe, RESTART_MODEL)[-1].index
    )

    final = probe.history(resumed)
    assert [message["content"] for message in final.messages()] == [
        COMPACT_CONTENT,
        "beta",
        SUMMARY_LOOKING_TEXT,
        "gamma",
        "post compact reply",
    ], final.describe()


#: 保留区事实场景的私有 model 名。
TAIL_MODEL = "probe/auto-compact-tail-facts"


@pytest.mark.probe_env(
    models=[TAIL_MODEL],
    context_window_tokens=CONTEXT_WINDOW,
    keep_recent_tokens=KEEP_RECENT,
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_compact_relink_keeps_tail_message_facts(probe: Probe) -> None:
    """apply 只换保留区（tail）的链坐标：上下文事实与审计字段都随行（红线）。

    WHEN 后台压缩换入（与 ``test_early_compact_persists_then_applies`` 同一条
    触发路径）
    THEN 保留区里被重链接的 assistant 记录：上下文事实与 apply 前逐字段等价
    （``message_semantics``，uuid 是新的），且 ``stop_reason`` / ``usage`` 仍在
    ——"重链接"是换链坐标，不是重造消息（截断审计不得因压缩凭空消失）。
    """
    probe.register(TAIL_MODEL, *_script())
    session = await probe.session(model=TAIL_MODEL)

    await session.chat("alpha")
    await session.chat("beta")
    await _wait_pending(probe, session)

    before_apply = probe.history(session)
    retained_before = [
        message
        for message in before_apply.messages()
        if message["role"] == "assistant" and message["content"] == SUMMARY_LOOKING_TEXT
    ]
    assert len(retained_before) == 1, before_apply.describe()
    original = retained_before[0]
    assert original.get("stop_reason") == "stop", original
    assert original["usage"]["prompt_tokens"] == SERVER_TOKENS, original

    result = await session.chat("gamma")
    assert result.data["result"] == "post compact reply", result.data

    final = probe.history(session)
    retained_after = [
        message
        for message in final.messages()
        if message["role"] == "assistant" and message["content"] == SUMMARY_LOOKING_TEXT
    ]
    assert len(retained_after) == 1, final.describe()
    relinked = retained_after[0]

    # 链坐标换了（uuid 全新），上下文事实逐字段等价（比较口径同红线断言）。
    assert relinked["uuid"] != original["uuid"], relinked
    assert message_semantics(relinked) == message_semantics(original), (
        relinked,
        original,
    )
    # provider 审计字段跟随：stop_reason / usage 不缺（截断审计在压缩后仍可读）。
    assert relinked.get("stop_reason") == "stop", relinked
    assert relinked["usage"]["prompt_tokens"] == SERVER_TOKENS, relinked
    final.assert_chain_invariants()
