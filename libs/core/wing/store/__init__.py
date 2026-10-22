"""wing/store — session 持久化层。

SessionStore 是一个 session 全部持久状态的唯一所有者，
MessageLog 是消息日志的耐久语义，TrackedList 组合 MessageLog 做 I/O 委托。

本包导出「抽象 + 模型 + 构造入口 + 落盘规范形」；MessageLog 的实现类
（FileMessageLog / MemoryMessageLog）是直测耐久语义时才需要的实现细节，经
``wing.store.file`` / ``wing.store.memory`` 直达。
"""

from wing.store.base import MessageLog, SessionStore
from wing.store.file import FileSessionStore
from wing.store.memory import MemorySessionStore
from wing.store.models import (
    ArchivedSession,
    FileRef,
    SessionMetadata,
    SessionSummary,
    SessionUsage,
    TagMeta,
)
from wing.store.names import validate_file_name, validate_media_id
from wing.store.records import canonical_record

__all__ = [
    "ArchivedSession",
    "FileRef",
    "MessageLog",
    "SessionMetadata",
    "SessionStore",
    "SessionSummary",
    "SessionUsage",
    "TagMeta",
    "canonical_record",
    "validate_file_name",
    "validate_media_id",
    "FileSessionStore",
    "MemorySessionStore",
]
