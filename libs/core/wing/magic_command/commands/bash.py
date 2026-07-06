# wing/magic_command/commands/bash.py

from typing import TYPE_CHECKING

from wing.common.logger import log

from ..registry import magic_registry
from ..shell import execute_shell

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(
    name="bash", aliases=["b", "sh"], description="执行 shell 命令", params="<command>"
)
async def cmd_bash(agent: "WingAgent", args: str) -> str:
    log.info(f"执行bash命令: {args}")
    return await execute_shell(command=args, timeout=60)
