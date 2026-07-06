"""魔术命令模块."""

# 注册内置命令（commands 包下的每个文件都含 @magic_registry.register 装饰器）
from .commands import *  # noqa: F401,F403
from .prompt_commands import (
    load_prompt_command_from_file,
    load_prompt_commands_from_paths,
    register_prompt_commands,
)
from .registry import MagicCommand, MagicCommandRegistry, magic_registry
from .shell import execute_shell

__all__ = [
    "MagicCommand",
    "MagicCommandRegistry",
    "magic_registry",
    "execute_shell",
    "load_prompt_command_from_file",
    "load_prompt_commands_from_paths",
    "register_prompt_commands",
]
