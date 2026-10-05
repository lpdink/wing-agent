"""workspace_env_inject hook — 在 session 启动时注入 workspace 和环境信息。

读取 session.session_workspace（fallback 到 os.getcwd()）和操作系统信息，
经 context_manager.append_to_system_prompt() 追加进系统提示词；追加内容随
会话持久化（metadata.append_system_prompt），resume / fork 后系统提示词
逐字节不变——KV cache 前缀在重建 agent 后仍然命中。
"""

from __future__ import annotations
import os
import platform
from typing import TYPE_CHECKING

from wing.hooks import HookRegistry, hooks

if TYPE_CHECKING:
    from wing.session import Session


@hooks.on("before_session_start")
def inject_workspace_env(session: "Session", **ctx) -> None:
    """注入 workspace 和环境信息到 system prompt。"""
    workspace = session.session_workspace or None
    os_info = f"{platform.system()} {platform.release()}"

    cm = session.context_manager
    if isinstance(workspace, str):
        cm.append_to_system_prompt(
            f"<current_directory>{workspace}</current_directory>"
        )
    cm.append_to_system_prompt(
        f"<os>{os_info}</os>\n<shell>{os.environ.get('SHELL', '/bin/sh')}</shell>"
    )


def register_workspace_env_inject(hooks: HookRegistry) -> None:
    """注册 workspace_env_inject hook 到指定 HookRegistry。"""
    hooks.on("before_session_start")(inject_workspace_env)
