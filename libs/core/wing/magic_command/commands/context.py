# wing/magic_command/commands/context.py

from typing import TYPE_CHECKING

from wing.common.token_counter import TokenCounter

from ..registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(
    name="context", aliases=["ctx"], description="查看上下文统计和系统提示词"
)
async def cmd_context(agent: "WingAgent", args: str) -> str:
    count, tokens = agent.context_manager.get_context_stats()
    system_prompt = agent.context_manager.system_prompt

    parts_info = []
    parts_info.append("工具列表：")
    parts_info.append(", ".join(list(agent._tool_map.keys())))
    parts_info.append("📊 上下文统计:")
    parts_info.append(f"  - 消息数: {count}")
    parts_info.append(f"  - 估计 tokens: {tokens}")
    parts_info.append("")
    parts_info.append("📝 系统提示词组成:")

    if agent.context_manager.setin_system_prompt:
        parts_info.append(
            f"  1. config.system_prompt ({TokenCounter.count(agent.context_manager.setin_system_prompt)} Tokens)"
        )
    if agent.context_manager._rules_prompt:
        parts_info.append(
            f"  2. rules ({TokenCounter.count(agent.context_manager._rules_prompt)} Tokens)"
        )
    if agent.context_manager._skills_prompt:
        parts_info.append(
            f"  3. skills ({TokenCounter.count(agent.context_manager._skills_prompt)} Tokens)"
        )

    parts_info.append("")
    parts_info.append("─" * 40)
    parts_info.append("完整系统提示词:")
    parts_info.append(system_prompt.content)

    return "\n".join(parts_info)
