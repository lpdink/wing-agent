# wing/magic_command/commands/rewind.py

from typing import TYPE_CHECKING

from wing.event import BranchTargetInfo, BranchTargetsEvent, SyncSessionEvent

from ..registry import magic_registry
from .utils import emit_context_stats

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(
    name="rewind",
    aliases=["rw"],
    description="回退到指定用户消息",
    params="[uuid|list]",
)
async def cmd_rewind(agent: "WingAgent", args: str) -> str:
    cm = agent.context_manager
    if not args or args.strip() == "list":
        targets = cm.get_branch_targets()
        if not targets:
            return "❌ 没有可回退的用户消息"
        agent.emit(
            BranchTargetsEvent(
                session_id=agent.session_id,
                targets=[BranchTargetInfo(**t) for t in targets],
            )
        )
        lines = ["📋 可回退的用户消息:"]
        for item in targets:
            lines.append(f"  {item['uuid']}  {item['content']}")
        lines.append("\n使用 /rewind <uuid> 回退到指定消息")
        return "\n".join(lines)

    target_uuid = args.strip()
    try:
        draft = cm.rewind(target_uuid)
        emit_context_stats(agent)
        # rewind 后把原用户消息作为 draft 给前端，方便用户继续编辑
        if draft:
            agent.emit(
                SyncSessionEvent(
                    session_id=agent.session_id,
                    messages=[msg.model_dump() for msg in cm.get_context_window()],
                    agent=None,
                    draft=draft,
                )
            )
        # 刷新前端 branch targets 缓存（rewind 改变了链结构）
        targets = cm.get_branch_targets()
        agent.emit(
            BranchTargetsEvent(
                session_id=agent.session_id,
                targets=[BranchTargetInfo(**t) for t in targets],
            )
        )
        return f"✅ 已回退到 uuid={target_uuid}"
    except ValueError as e:
        return f"❌ {e}"
