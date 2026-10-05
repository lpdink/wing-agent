# wing/context/__init__.py
"""wing/context 包 — 上下文窗口、压缩与 skills/rules 资源加载。

公共 API 通过此 __init__ re-export：
    from wing.context import ContextManager, Compactor, PendingCompact, LLMMessagesResult

内部模块：``manager``（ContextManager —— 窗口投影 / 声明集 / rewind / 压缩编排）·
``compaction``（Compactor —— 压缩策略 + 压缩链路上的数据类）·
``resources``（skills/rules 文件加载，与消息链无关）。
"""

from .compaction import Compactor, LLMMessagesResult, PendingCompact
from .manager import ContextManager

__all__ = [
    "ContextManager",
    "Compactor",
    "PendingCompact",
    "LLMMessagesResult",
]
