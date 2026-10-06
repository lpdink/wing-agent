"""wing/store — session 持久化层。

SessionStore 是一个 session 全部持久状态的唯一所有者，
MessageLog 是消息日志的耐久语义，TrackedList 组合 MessageLog 做 I/O 委托。

本包只导出「抽象 + 构造入口」；MessageLog 的实现类（FileMessageLog /
MemoryMessageLog）是直测耐久语义时才需要的实现细节，经
``wing.store.file`` / ``wing.store.memory`` 直达。
"""

from wing.store.base import (
    MessageLog,
    SessionMetadata,
    SessionStore,
    SessionSummary,
    TagMeta,
)
from wing.store.file import FileSessionStore
from wing.store.memory import MemorySessionStore

__all__ = [
    "MessageLog",
    "SessionMetadata",
    "SessionStore",
    "SessionSummary",
    "TagMeta",
    "FileSessionStore",
    "MemorySessionStore",
]
