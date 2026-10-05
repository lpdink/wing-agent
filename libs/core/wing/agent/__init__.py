# wing/agent/__init__.py
"""wing/agent 包 — WingAgent 及其内部协作模块。

公共 API 通过此 __init__ re-export，外部导入路径不变：
    from wing.agent import WingAgent, ToolContext, current_tool_call_id

内部队列 DTO（Inbound）不是公共 API，经 `wing.agent.inbox` 直达。
"""

from .core import WingAgent
from .tool_context import ToolContext
from .tool_executor import current_tool_call_id

__all__ = [
    "WingAgent",
    "ToolContext",
    "current_tool_call_id",
]
