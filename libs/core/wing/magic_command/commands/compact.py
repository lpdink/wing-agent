# wing/magic_command/commands/compact.py

from typing import TYPE_CHECKING

from wing.event import CompactDoneEvent
from wing.schema import Message

from ..registry import magic_registry
from .utils import emit_context_stats

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(name="compact", aliases=["cp"], description="压缩上下文")
async def cmd_compact(agent: "WingAgent", args: str) -> str:
    cm = agent.context_manager
    if not cm.compactor:
        return "❌ 未配置 compaction helper"
    # 手动 compact 前丢弃任何 pending async compact
    cm._discard_pending_compact()
    msgs = list(cm._messages)
    try:
        # 构造完整上下文（同主 agent 调用前缀），最大化缓存命中
        full_context = [cm.system_prompt] + msgs
        compacted = await cm.compactor.do_compact(
            full_context,
            agent.model,
            agent.model_provider,
            tools=agent.tools,
        )
        # 构造压缩节点（全部消息被压缩）
        last_compressed_uuid = msgs[-1].uuid if msgs else None
        compact_node = Message(
            role="assistant",
            content=compacted.content,
            parent_uuid=None,
            unzip_last_uuid=last_compressed_uuid,
        )
        compact_node.uuid = __import__("uuid").uuid4().hex

        # 写入 JSONL
        cm._messages.append_detached(compact_node)
        cm._messages.set_tip(compact_node.uuid)

        agent.emit(
            CompactDoneEvent(
                session_id=agent.session_id,
                original_tokens=compacted.usage.prompt_tokens,
                compressed_tokens=compacted.usage.completion_tokens,
                model=agent.model,
            )
        )
        emit_context_stats(agent)
        return (
            f"✅ Compact 完成!\n"
            f"  - 原始消息数: {len(msgs)}\n"
            f"  - 原始tokens: {compacted.usage.prompt_tokens}\n"
            f"  - 压缩后tokens: {compacted.usage.completion_tokens}"
        )
    except Exception:
        return "压缩失败."
