# wing/session/__init__.py
"""wing.session 包 — 会话生命周期。

公共 API 通过此 __init__ re-export：

    from wing.session import (
        Session, serialize_message, tool_refs,
        SessionManager, SessionReaper, SessionArchiver,
        AgentTemplate, AgentTemplateManager,
        ModelBinding,
        AgentOverride,
    )

内部模块：``session``（Session —— 单会话身份 / 元数据 / 状态变更 / post）·
``manager``（SessionManager —— 多会话 / fork / resume / 逐出 / 归档）·
``reaper``（SessionReaper —— 空闲会话逐出）·
``archiver``（SessionArchiver —— 过期会话自动归档）·
``template``（AgentTemplate —— 配置 agents: 的 model/tools/prompt/skills/rules）·
``model_binding``（生效模型绑定 —— resume 链的只读投影，会话列表的模型列）·
``override``（AgentOverride —— 创建期参数覆盖的领域类型）。
"""

from .archiver import SessionArchiver
from .manager import SessionArchivedError, SessionManager
from .model_binding import ModelBinding
from .override import AgentOverride
from .reaper import SessionReaper
from .session import Session, serialize_message, tool_refs
from .tags import TagMutation
from .template import AgentTemplate, AgentTemplateManager

__all__ = [
    "AgentOverride",
    "AgentTemplate",
    "AgentTemplateManager",
    "ModelBinding",
    "Session",
    "SessionArchivedError",
    "SessionArchiver",
    "SessionManager",
    "SessionReaper",
    "TagMutation",
    "serialize_message",
    "tool_refs",
]
