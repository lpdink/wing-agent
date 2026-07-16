"""Prompt 命令模块——加载、注册、展开。"""

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
