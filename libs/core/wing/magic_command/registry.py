"""Prompt 命令注册表——仅存储元数据，不做分发。"""

from __future__ import annotations

from typing import Optional

from pydantic import BaseModel


class MagicCommand(BaseModel):
    """命令元数据定义。"""

    name: str
    """命令名，如 'plan'"""

    aliases: list[str] = []
    """别名列表"""

    description: str = ""
    """描述文本"""

    params: str = ""
    """参数说明，如 '[args]'"""

    source: str = "prompt"
    """命令来源: 当前只支持 'prompt' (用户 md 文件)"""

    file_path: Optional[str] = None
    """prompt 类型命令的 .md 文件路径"""


class MagicCommandRegistry:
    """命令元数据注册表。"""

    def __init__(self) -> None:
        self._commands: dict[str, MagicCommand] = {}

    def get(self, name: str) -> Optional[MagicCommand]:
        """获取命令。"""
        return self._commands.get(name)

    def register_command(self, cmd: MagicCommand) -> None:
        """注册一个 MagicCommand（主命令名 + 别名）。"""
        self._commands[cmd.name] = cmd
        for alias in cmd.aliases:
            self._commands[alias] = cmd

    def remove_by_source(self, source: str) -> int:
        """移除指定来源的所有命令（含别名），返回移除的独立命令数。"""
        to_remove = {
            id(cmd) for _, cmd in self._commands.items() if cmd.source == source
        }
        self._commands = {
            n: c for n, c in self._commands.items() if id(c) not in to_remove
        }
        return len(to_remove)

    def list_all(self) -> list[MagicCommand]:
        """获取所有命令列表（去重）。"""
        seen: set[str] = set()
        result: list[MagicCommand] = []
        for cmd in self._commands.values():
            if cmd.name not in seen:
                seen.add(cmd.name)
                result.append(cmd)
        return result


# 全局实例
magic_registry = MagicCommandRegistry()
