# wing/session/__init__.py
"""wing.session 包 — 会话生命周期。

公共 API 通过此 __init__ re-export：

    from wing.session import (
        Session, serialize_message, tool_refs,
        SessionManager, SessionReaper,
        AgentTemplate, AgentTemplateManager,
        AgentOverride,
    )

内部模块：``session``（Session —— 单会话身份 / 元数据 / 状态变更 / post）·
``manager``（SessionManager —— 多会话 / fork / resume / 逐出）·
``reaper``（SessionReaper —— 空闲会话逐出）·
``template``（AgentTemplate —— 配置 agents: 的 model/tools/prompt/skills/rules）·
``override``（AgentOverride —— 创建期参数覆盖的领域类型）。
"""

from .manager import SessionManager
from .override import AgentOverride
from .reaper import SessionReaper
from .session import Session, serialize_message, tool_refs
from .tags import TagMutation
from .template import AgentTemplate, AgentTemplateManager

__all__ = [
    "AgentOverride",
    "AgentTemplate",
    "AgentTemplateManager",
    "Session",
    "SessionManager",
    "SessionReaper",
    "TagMutation",
    "serialize_message",
    "tool_refs",
]
