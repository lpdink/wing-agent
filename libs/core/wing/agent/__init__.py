# wing/agent/__init__.py
"""wing/agent 包 — WingAgent 及其内部协作模块。

公共 API 通过此 __init__ re-export，外部导入路径不变：
    from wing.agent import WingAgent, ToolContext, current_tool_call_id, Inbound
"""

from .core import WingAgent
from .inbox import Inbound
from .tool_context import ToolContext
from .tool_executor import current_tool_call_id

__all__ = [
    "WingAgent",
    "ToolContext",
    "Inbound",
    "current_tool_call_id",
]
