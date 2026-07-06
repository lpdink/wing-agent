"""魔术命令注册表."""

from __future__ import annotations

import inspect
from typing import Any, Callable, Optional

from pydantic import BaseModel, ConfigDict


class MagicCommand(BaseModel):
    """魔术命令定义."""

    model_config = ConfigDict(arbitrary_types_allowed=True)

    name: str
    """命令名，如 'model'"""

    aliases: list[str] = []
    """别名，如 ['m']"""

    description: str = ""
    """描述文本"""

    params: str = ""
    """参数说明，如 '[name]'"""

    handler: Callable[..., Any]
    """处理函数，签名: async (agent: 'WingAgent', args: str) -> str"""

    source: str = "builtin"
    """命令来源: 'builtin' (内置) 或 'prompt' (用户 md 文件)"""


class MagicCommandRegistry:
    """魔术命令注册表."""

    def __init__(self):
        self._commands: dict[str, MagicCommand] = {}

    def register(
        self,
        name: str | None = None,
        aliases: list[str] | None = None,
        description: str = "",
        params: str = "",
    ):
        """装饰器方式注册命令。

        用法:
            @magic_registry.register(name="help", aliases=["h", "?"], description="显示帮助")
            async def cmd_help(agent, args: str) -> str:
                ...
        """

        def decorator(fn: Callable[..., Any]) -> Callable[..., Any]:
            cmd = MagicCommand(
                name=name or getattr(fn, "__name__", "unknown"),
                aliases=aliases or [],
                description=description or inspect.getdoc(fn) or "",
                params=params,
                handler=fn,
            )
            self.register_command(cmd)
            return fn

        return decorator

    def get(self, name: str) -> Optional[MagicCommand]:
        """获取命令."""
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
        """获取所有命令列表（去重）."""
        seen: set[str] = set()
        result: list[MagicCommand] = []
        for cmd in self._commands.values():
            if cmd.name not in seen:
                seen.add(cmd.name)
                result.append(cmd)
        return result

    def completions(self, prefix: str) -> list[str]:
        """获取补全列表."""
        if not prefix.startswith("/"):
            return []
        return [
            f"/{c.name}" for c in self.list_all() if f"/{c.name}".startswith(prefix)
        ]

    def get_suggestions(self, prefix: str) -> list[dict[str, str]]:
        """获取候选命令详细信息，用于 TUI 实时显示（支持大小写不敏感匹配）。

        Args:
            prefix: 用户输入的前缀，如 "/" 或 "/h"

        Returns:
            [{'name': 'help', 'aliases': 'h, ?', 'description': '...', 'params': ''}, ...]
        """
        if not prefix.startswith("/"):
            return []

        search_term = prefix.lstrip("/").lower()  # 转为小写
        results = []

        for cmd in self.list_all():
            # 大小写不敏感匹配命令名或别名
            if cmd.name.lower().startswith(search_term) or any(
                a.lower().startswith(search_term) for a in cmd.aliases
            ):
                aliases_str = ", ".join(cmd.aliases) if cmd.aliases else ""
                results.append(
                    {
                        "name": cmd.name,
                        "aliases": aliases_str,
                        "description": cmd.description,
                        "params": cmd.params,
                    }
                )

        return results

    def help_text(self) -> str:
        """生成帮助文本."""
        lines = ["📖 魔术命令帮助:"]
        for cmd in self.list_all():
            params = f" {cmd.params}" if cmd.params else ""
            aliases = f" ({', '.join(cmd.aliases)})" if cmd.aliases else ""
            lines.append(f"  /{cmd.name}{params} - {cmd.description}{aliases}")
        return "\n".join(lines)


# 全局实例
magic_registry = MagicCommandRegistry()
