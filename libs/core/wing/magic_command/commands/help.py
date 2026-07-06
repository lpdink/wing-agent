# wing/magic_command/commands/help.py

from typing import TYPE_CHECKING

from wing.event import CommandInfo, CommandListEvent

from ..registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(name="help", aliases=["h", "?"], description="显示帮助信息")
async def cmd_help(agent: "WingAgent", args: str) -> str:
    agent.emit(
        CommandListEvent(
            session_id=agent.session_id,
            commands=[
                CommandInfo(
                    name=cmd.name,
                    aliases=cmd.aliases,
                    description=cmd.description,
                    params=cmd.params,
                )
                for cmd in magic_registry.list_all()
            ],
        )
    )
    return magic_registry.help_text()
