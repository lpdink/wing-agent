"""workspace_env_inject hook — 在 session 启动时注入 workspace 和环境信息。

读取 session.session_workspace（fallback 到 os.getcwd()）和操作系统信息，
注入到 session.context_manager.inject_system_prompts。
"""

from __future__ import annotations
import os
import platform
from typing import TYPE_CHECKING

from wing.hook_registry import HookRegistry, hooks

if TYPE_CHECKING:
    from wing.session import Session


@hooks.on("before_session_start")
def inject_workspace_env(session: "Session", **ctx) -> None:
    """注入 workspace 和环境信息到 system prompt。"""
    workspace = session.session_workspace or None
    os_info = f"{platform.system()} {platform.release()}"

    prompts = session.context_manager.inject_system_prompts
    if isinstance(workspace, str):
        prompts.append(f"<current_directory>{workspace}</current_directory>")
    prompts.append(
        f"<os>{os_info}</os>\n<shell>{os.environ.get('SHELL', '/bin/sh')}</shell>"
    )


def register_workspace_env_inject(hooks: HookRegistry) -> None:
    """注册 workspace_env_inject hook 到指定 HookRegistry。"""
    hooks.on("before_session_start")(inject_workspace_env)
