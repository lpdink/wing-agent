"""Session 生命周期命令（占位）——用于帮助和自动补全。

实际处理在 SM._post() 中通过 magic_registry 路由，
这里的 handler 不会被直接调用。仅占位供 magic_registry 列表显示。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from wing.magic_command.registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(
    name="new",
    description="创建新会话",
    params="[name]",
)
async def cmd_new(agent: "WingAgent", args: str) -> str:
    """占位——实际由 SM._dispatch_session_command 处理。"""
    return ""


@magic_registry.register(
    name="session",
    aliases=["ss"],
    description="切换会话或列出会话",
    params="[session_id]",
)
async def cmd_session(agent: "WingAgent", args: str) -> str:
    """占位——实际由 SM._dispatch_session_command 处理。"""
    return ""


@magic_registry.register(
    name="fork",
    aliases=[],
    description="从指定消息分叉出新会话",
    params="<uuid>",
)
async def cmd_fork(agent: "WingAgent", args: str) -> str:
    """占位——实际由 SM._dispatch_session_command 处理。"""
    return ""
