# wing/magic_command/commands/interrupt.py

from typing import TYPE_CHECKING

from wing.event import InterruptedEvent

from ..registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(name="interrupt", aliases=["int"], description="中断当前任务")
async def cmd_interrupt(agent: "WingAgent", args: str) -> str:
    agent.interrupt()
    agent.emit(InterruptedEvent(session_id=agent.session_id))
    return "✅ Agent 已中断，inbox 已清空，事件循环已重置"
