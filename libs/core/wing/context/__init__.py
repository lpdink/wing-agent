# wing/context/__init__.py
"""wing/context 包 — 上下文窗口、压缩、skills/rules 加载与缓存账本。

公共 API 通过此 __init__ re-export：
    from wing.context import ContextManager, Compactor, PendingCompact, LLMMessagesResult
    from wing.context import CacheLedger, build_report, empty_report

内部模块：``manager``（ContextManager —— 窗口投影 / 声明集 / rewind / 压缩编排）·
``compaction``（Compactor —— 压缩策略 + 压缩链路上的数据类；``resolve_context_tokens``
是 token 口径的唯一实现，压缩阈值与 context stats 共用）·
``resources``（skills/rules 文件加载，与消息链无关）·
``cache_ledger``（缓存账本 —— 分段指纹 / 命中归因 / 端点投影）·
``cache_store``（账本的唯一 SQL 处：`cache_call` / `cache_segment` 读写）。
"""

from .cache_ledger import CacheLedger, build_report, empty_report
from .compaction import (
    Compactor,
    LLMMessagesResult,
    PendingCompact,
    resolve_context_tokens,
)
from .manager import ContextManager

__all__ = [
    "CacheLedger",
    "ContextManager",
    "Compactor",
    "LLMMessagesResult",
    "PendingCompact",
    "build_report",
    "resolve_context_tokens",
    "empty_report",
]
