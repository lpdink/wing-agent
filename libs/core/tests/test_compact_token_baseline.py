"""压缩换入后的 token 口径（issue #247）。

`_apply_pending_compact` 重链接保留区时整条复制消息，`usage` 随行（审计事实，有意保留）。
于是「换入之后、下一次带真实 usage 的 assistant 落链之前」，链上最后一条带 usage 的
assistant 携带的是**压缩前**窗口的 `prompt_tokens`——那个窗口里还包含刚被摘要掉的整段
head。`_last_prompt_tokens` 是 `resolve_context_tokens` 的唯一来源（压缩双阈值 +
`context_stats` 同源），读到它就等于把压缩前的 window 当成当前 context 大小：

- `need_early_trigger` 假阳性 → 起一次多余的后台压缩，链已被压小、切点落在链尾 →
  产出「整条链的摘要」；
- 那个值同时 ≥ `context_window_tokens` → `need_apply_compact` 也假阳性 → 下一个请求
  把当时的整条链换成一个摘要节点（用户可感知为「对话被吞掉」）。

本文件用真实的请求组装路径（`get_messages_for_llm` → `_last_prompt_tokens`）复现这条
序列，不手工摆私有状态。阈值按千级设置，让 early / apply 在几步内确定性触发。
"""

from __future__ import annotations

import uuid
from pathlib import Path
from typing import Any, AsyncIterator

import pytest

from _support import make_cm
from wing.context import Compactor, ContextManager
from wing.schema import ChainNode, LLMResponse, LLMUsage, Message

#: 双阈值（apply = 窗口，early = 窗口 − 保留区 = 600）。
CONTEXT_WINDOW = 1000
KEEP_RECENT = 400
COMPACT_WINDOW = CONTEXT_WINDOW - KEEP_RECENT

#: 压缩前窗口的服务端口径（跨过两个阈值）。
PRE_COMPACT_TOKENS = 1100

#: 压缩产物（`do_compact` 产出的内容形状：`[Compact] <summary>`）。
COMPACT_SUMMARY = "[Compact] head summary"

SESSION_ID = "test-compact-tokens"

#: 本文件构造的 CM（用例失败时会留下后台 task，teardown 统一收尾）。
_CREATED: list[ContextManager] = []


@pytest.fixture(autouse=True)
def _cancel_leftover_tasks():
    yield
    for cm in _CREATED:
        task = cm._pending_compact_task
        if task is not None and not task.done():
            task.cancel()
    _CREATED.clear()


def _make_cm(tmp_dir: Path) -> ContextManager:
    cm = make_cm(
        tmp_dir,
        session_id=SESSION_ID,
        compactor=Compactor(
            context_window_tokens=CONTEXT_WINDOW, keep_recent_tokens=KEEP_RECENT
        ),
    )
    _CREATED.append(cm)
    return cm


def _provider() -> Any:
    """压缩调用的 provider 替身（只服务后台 compact 的非流式请求）。"""

    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
        options: object = None,
    ) -> AsyncIterator[LLMResponse]:
        yield LLMResponse(
            content="<summary>head summary</summary>",
            usage=LLMUsage(prompt_tokens=200, completion_tokens=20),
        )

    return type("MockProvider", (), {"generate": _gen})()


async def _assemble(cm: ContextManager) -> list[Message]:
    """一次请求组装（走真实路径，返回本次要给 LLM 的窗口）。"""
    result = await cm.get_messages_for_llm(
        model="m", model_provider=_provider(), current_tools=lambda: []
    )
    return result.messages


def _turn_reply(text: str, prompt_tokens: int = PRE_COMPACT_TOKENS) -> Message:
    """一轮主调用的 assistant 落链（真实 usage = 服务端回带的 prompt_tokens）。"""
    return Message(
        role="assistant",
        content=text,
        usage=LLMUsage(prompt_tokens=prompt_tokens, completion_tokens=20),
    )


async def _drive_to_applied(cm: ContextManager) -> None:
    """把链推进到「摘要已换入」，且保留区里带着压缩前的 usage。

    时间线（与真实自动压缩一致）：

    1. 首轮：无服务端 usage → 估算远低于阈值，不触发；
    2. 第二轮组装：口径 1100 ≥ compact_window(600) → 起后台 early compact；
    3. 这一轮的回复落链（**压缩前**窗口的 usage，1100）；
    4. 第三轮组装：1100 ≥ context_window(1000) → 换入摘要 → 压缩节点 + 重链接
       保留区，保留区里那条 assistant 仍带 1100。
    """
    cm.add_message(Message(role="user", content="alpha"))
    await _assemble(cm)  # 1

    cm.add_message(_turn_reply("reply one"))
    await _assemble(cm)  # 2 → early compact 触发
    assert cm._pending_compact_task is not None, "early compact 没被触发"
    await cm._pending_compact_task
    assert cm._pending_compact_result is not None

    cm.add_message(Message(role="user", content="beta"))
    cm.add_message(_turn_reply("beta reply"))

    window = await _assemble(cm)  # 3 → 换入摘要
    contents = [m.content for m in window]
    assert contents[1] == COMPACT_SUMMARY, contents
    assert contents[2:] == ["beta", "beta reply"], contents
    assert cm._pending_compact_result is None


@pytest.mark.asyncio
async def test_apply_drops_pre_compact_prompt_tokens(tmp_dir):
    """换入之后，口径不得停在压缩前的 prompt_tokens（issue #247 的复现）。"""
    cm = _make_cm(tmp_dir)
    await _drive_to_applied(cm)

    # 换入之后、下一轮真实 usage 落链之前：压缩前的值不能当作当前 context 大小。
    assert cm._last_prompt_tokens() is None

    count, tokens = cm.get_context_stats()
    assert tokens < COMPACT_WINDOW, (count, tokens)
    # 链已经变小：这个口径下两个阈值都不该被跨过。
    compactor = cm.compactor
    assert compactor is not None
    window = cm.get_context_window()
    assert not compactor.need_early_trigger(window, cm._last_prompt_tokens())
    assert not compactor.need_apply_compact(window, cm._last_prompt_tokens())


@pytest.mark.asyncio
async def test_no_spurious_compact_when_post_apply_turn_has_no_usage(tmp_dir):
    """apply 之后那一轮没有真实 usage → 不得再压一次、不得把整条链换成摘要。

    真实形态：上游不回带 usage 的兼容网关 / 中断收场的轮次——落链的 assistant
    的 `usage.prompt_tokens` 为 0，`_last_prompt_tokens` 跳过它，于是**它之前**那条
    重链接的 assistant 成了「最后一条带 usage 的消息」。
    """
    cm = _make_cm(tmp_dir)
    await _drive_to_applied(cm)

    # apply 之后的那一轮：没有真实 usage（prompt_tokens = 0 被口径跳过）。
    cm.add_message(_turn_reply("post compact reply", prompt_tokens=0))

    # 下一次组装：口径若仍停在 1100，就会起一次多余的后台压缩（切点落在链尾
    # → 整条链的摘要），并在再下一次组装时把整条链换掉。
    await _assemble(cm)
    assert cm._pending_compact_task is None
    assert cm._pending_compact_result is None

    cm.add_message(Message(role="user", content="delta"))
    contents = [m.content for m in await _assemble(cm)]
    assert "beta" in contents, contents
    assert "post compact reply" in contents, contents
    assert sum(1 for c in contents if c and c.startswith("[Compact]")) == 1, contents


# ── 边界的降级 / 持久化语义（design D1–D4）──────────────


def _compact_node(
    unzip_uuid: str | None,
    *,
    boundary: str | None = None,
    content: str = COMPACT_SUMMARY,
) -> Message:
    """造一个压缩节点（`parent_uuid=None` + `unzip_last_uuid`，活跃链的根）。"""
    node = Message(
        role="assistant",
        content=content,
        parent_uuid=None,
        unzip_last_uuid=unzip_uuid,
        usage_boundary_uuid=boundary,
    )
    node.uuid = uuid.uuid4().hex
    return node


def _reload(tmp_dir: Path) -> ContextManager:
    """按加载路径重建 CM（= resume / 进程重启后的口径来源：**只有磁盘事实**）。"""
    from wing.chain import TrackedList
    from wing.store.file import FileMessageLog

    messages: TrackedList[ChainNode] = TrackedList.load(
        FileMessageLog(tmp_dir / SESSION_ID), Message
    )
    cm = ContextManager(
        session_id=SESSION_ID,
        messages=messages,
        system_prompt="You are a helpful assistant.",
        compactor=Compactor(
            context_window_tokens=CONTEXT_WINDOW, keep_recent_tokens=KEEP_RECENT
        ),
    )
    _CREATED.append(cm)
    return cm


@pytest.mark.asyncio
async def test_boundary_survives_reload(tmp_dir):
    """reload / resume：边界在链上（不是内存态），重建后口径一致。"""
    cm = _make_cm(tmp_dir)
    await _drive_to_applied(cm)
    assert cm._last_prompt_tokens() is None

    reloaded = _reload(tmp_dir)
    assert [m.content for m in reloaded.get_context_window()] == [
        m.content for m in cm.get_context_window()
    ]
    assert reloaded._last_prompt_tokens() is None, "边界没落盘 → 口径回落到大值"

    # 重建后下一轮真实 usage 落链 → 立刻回到服务端口径。
    reloaded.add_message(_turn_reply("delta reply", prompt_tokens=300))
    assert reloaded._last_prompt_tokens() == 300


@pytest.mark.asyncio
async def test_manual_compact_sets_its_own_boundary(tmp_dir):
    """手动压缩（整段链被摘要取代）：边界 = 节点自己，不产生"永久不采信"。"""
    cm = _make_cm(tmp_dir)
    cm.add_message(Message(role="user", content="alpha"))
    cm.add_message(_turn_reply("reply one"))

    await cm.do_manual_compact("m", _provider(), current_tools=lambda: [])

    window = cm.get_context_window()
    assert [m.content for m in window] == [COMPACT_SUMMARY]
    compact_node = window[0]
    assert compact_node.usage_boundary_uuid == compact_node.uuid
    assert cm._last_prompt_tokens() is None

    # 换入之后的 usage 都是新事实（边界没有把它一起否掉）。
    cm.add_message(_turn_reply("post compact reply", prompt_tokens=300))
    assert cm._last_prompt_tokens() == 300


@pytest.mark.asyncio
async def test_second_compaction_rebases_boundary(tmp_dir):
    """第二次压缩：新 tail 里含上一次的压缩节点，边界跟着新的换入时刻走。"""
    cm = _make_cm(tmp_dir)
    await _drive_to_applied(cm)

    # 窗口再次涨到 1100（真实服务端口径）→ 第二次 early compact → 第二次 apply。
    cm.add_message(Message(role="user", content="gamma"))
    cm.add_message(_turn_reply("gamma reply"))
    await _assemble(cm)
    assert cm._pending_compact_task is not None
    await cm._pending_compact_task

    cm.add_message(Message(role="user", content="epsilon"))
    cm.add_message(_turn_reply("epsilon reply"))
    contents = [m.content for m in await _assemble(cm)]
    assert contents[1:] == [COMPACT_SUMMARY, "epsilon", "epsilon reply"], contents

    # 新的压缩节点是根；边界 = 换入时刻的链尖（重链接 tail 的最后一条）。
    window = cm.get_context_window()
    second = window[0]
    assert second.unzip_last_uuid is not None
    assert second.usage_boundary_uuid == window[-1].uuid
    assert second.usage_boundary_uuid != second.uuid
    # 换入之后的口径不采信任何更早的 usage。
    assert cm._last_prompt_tokens() is None


@pytest.mark.asyncio
async def test_legacy_compact_node_keeps_historical_baseline(tmp_dir):
    """存量记录（本次修复前落盘、压缩节点没有边界指针）按历史口径降级（D4）。"""
    cm = _make_cm(tmp_dir)
    compressed = Message(role="user", content="alpha")
    cm.add_message(compressed)
    legacy = _compact_node(compressed.uuid)
    cm._messages.append_detached(legacy)
    cm._messages.set_tip(legacy.uuid)  # ty: ignore[invalid-argument-type]
    cm.add_message(Message(role="user", content="beta"))
    cm.add_message(_turn_reply("beta reply"))

    assert legacy.usage_boundary_uuid is None
    # 旧口径：压缩节点之后的 usage 仍被采信（存量记录证明不了，只能按旧行为）。
    assert cm._last_prompt_tokens() == PRE_COMPACT_TOKENS

    # 自愈路径：下一次压缩产出的节点带边界，口径从此按新规则判定。
    await cm.do_manual_compact("m", _provider(), current_tools=lambda: [])
    assert cm.get_context_window()[0].usage_boundary_uuid is not None
    assert cm._last_prompt_tokens() is None


def test_dangling_boundary_trusts_no_usage(tmp_dir):
    """声明的指针不在链上（rewind 剪断 / 日志损坏）→ 一条都不采信。"""
    cm = _make_cm(tmp_dir)
    head = Message(role="user", content="alpha")
    cm.add_message(head)
    compact = _compact_node(head.uuid, boundary="gone-with-the-rewind")
    cm._messages.append_detached(compact)
    cm._messages.set_tip(compact.uuid)  # ty: ignore[invalid-argument-type]
    cm.add_message(Message(role="user", content="u1"))
    cm.add_message(_turn_reply("stale reply"))  # 压缩前的窗口（1100）

    assert cm._last_prompt_tokens() is None


def test_rewind_into_tail_drops_stale_usage_and_recovers(tmp_dir):
    """回退进保留区：残留的保留区 usage 不信，回退之后的真实 usage 立刻生效。

    回退行自己声明"信任从这里恢复"——所以不是"永久退回估算"：下一轮真实 usage
    落链即回到服务端口径（rewind 是用户最常见的动作之一，代价得有界）。
    """
    cm = _make_cm(tmp_dir)
    head = Message(role="user", content="alpha")
    cm.add_message(head)

    # 先把边界节点（apply 时刻的链尖）建出来，压缩节点因此能在**落盘之前**带上它。
    tip = _turn_reply("tip reply", prompt_tokens=900)
    tip.uuid = uuid.uuid4().hex
    compact = _compact_node(head.uuid, boundary=tip.uuid)
    cm._messages.append_detached(compact)
    cm._messages.set_tip(compact.uuid)  # ty: ignore[invalid-argument-type]
    cm.add_message(Message(role="user", content="u1"))
    cm.add_message(_turn_reply("stale reply"))  # 压缩前的窗口（1100）
    cm.add_message(Message(role="user", content="u2"))
    cm.add_message(tip)
    cm.add_message(Message(role="user", content="u3"))
    cm.add_message(_turn_reply("newer reply", prompt_tokens=500))

    # 边界之后的 usage 被采信。
    assert cm._last_prompt_tokens() == 500

    # 回退到边界节点：边界记录离开活跃链（新 uuid 的复制行顶替），
    # 残留的 stale usage 不得复活。
    cm.rewind(tip.uuid)
    assert cm._last_prompt_tokens() is None
    # 但回退行声明了新的信任起点 → 之后落链的真实 usage 立刻被采信。
    cm.add_message(Message(role="user", content="after rewind"))
    cm.add_message(_turn_reply("recovered reply", prompt_tokens=300))
    assert cm._last_prompt_tokens() == 300


@pytest.mark.asyncio
async def test_empty_chain_manual_compact_declares_resumption(tmp_dir):
    """空链手动压缩：没有可压缩区间（`unzip_last_uuid` 为空）但仍是新链的起点。

    写入规则是统一的——任何把窗口重置到该节点的写入都声明"此后 usage 度量在当前
    窗口上"；读侧不看 `unzip_last_uuid`，所以这里不多一个特例。
    """
    cm = _make_cm(tmp_dir)
    await cm.do_manual_compact("m", _provider(), current_tools=lambda: [])

    node = cm.get_context_window()[0]
    assert node.content == COMPACT_SUMMARY
    assert node.unzip_last_uuid is None
    assert node.usage_boundary_uuid == node.uuid
    assert cm._last_prompt_tokens() is None
    # 之后落链的 usage 是当前窗口的事实（整条链就是这一条消息）。
    cm.add_message(_turn_reply("first reply", prompt_tokens=42))
    assert cm._last_prompt_tokens() == 42
