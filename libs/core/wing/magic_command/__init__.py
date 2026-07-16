"""魔术命令模块——仅保留 prompt 类型命令和注册表。"""

from .prompt_commands import (
    expand_prompt_command,
    load_prompt_command_from_file,
    load_prompt_commands_from_paths,
    register_prompt_commands,
)
from .registry import MagicCommand, MagicCommandRegistry, magic_registry

__all__ = [
    "MagicCommand",
    "MagicCommandRegistry",
    "magic_registry",
    "expand_prompt_command",
    "load_prompt_command_from_file",
    "load_prompt_commands_from_paths",
    "register_prompt_commands",
]
