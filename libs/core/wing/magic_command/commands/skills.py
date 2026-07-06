# wing/magic_command/commands/skills.py

from typing import TYPE_CHECKING

from ..registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(name="skills", description="显示当前安装的 skills")
async def cmd_skills(agent: "WingAgent", args: str) -> str:
    return agent.context_manager.get_skills_info()
