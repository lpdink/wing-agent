"""
wing/store/base.py — 持久化层抽象。

两个核心抽象：
- SessionStore: 一个 session 全部持久状态（metadata + 消息日志 + aux）的唯一所有者
- MessageLog: 消息日志的耐久语义（append-only 记录 + 活跃链快照 + aux kv）

关系（组合，非继承）：
- SessionStore 组合 MessageLog 工厂（open_log）
- TrackedList 组合 MessageLog 做 I/O 委托，自身只管链拓扑
- 除本包实现外，任何模块不得直接对存储介质做 I/O

接口刻意保持存储无关（无 path/fsync/glob 概念泄漏），
使 SQLite/PG/Supabase/Redis 后端可以极小成本实现。
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import Any

from pydantic import BaseModel, ConfigDict


class SessionMetadata(BaseModel):
    """Session 持久元数据。全字段可选，序列化时排除 None。

    TODO(future): 持久化更多动态状态（model、thinking、yolo 等）——
    可动态切换的状态将来都应可恢复，届时在此扩展字段。
    """

    model_config = ConfigDict(extra="ignore")

    session_name: str | None = None
    workspace: str | None = None
    last_interaction: str | None = None
    forked_from: str | None = None
    template_name: str | None = None


class SessionSummary(BaseModel):
    """session 列举条目：id + metadata + 标题回退素材。

    first_user_message: 快照中第一条用户消息（截断至 100 字符），
    仅在 metadata.session_name 为 None 时由后端填充，供上层做标题回退。
    """

    id: str
    metadata: SessionMetadata
    first_user_message: str | None = None


class MessageLog(ABC):
    """消息日志的耐久语义。传输单位为 raw dict 记录（schema 归 TrackedList 管）。

    耐久语义由后端定义：file 后端 fsync，memory 后端进程内，
    SQL 后端即一张 (session_id, seq, record jsonb) 表。
    """

    @abstractmethod
    def load_all(self) -> list[dict[str, Any]]:
        """按写入序返回全部记录。无记录返回空列表。

        损坏的记录行由后端跳过（存储完整性归后端管）。
        """

    @abstractmethod
    def append(self, records: list[dict[str, Any]]) -> None:
        """批量追加记录（append-only，永不修改已有记录）。空列表为 NOP。"""

    @abstractmethod
    def write_snapshot(self, records: list[dict[str, Any]]) -> None:
        """写入活跃链的人类可读快照。无快照概念的后端 NOP。"""

    @abstractmethod
    def read_aux(self, key: str) -> dict[str, Any] | None:
        """读取辅助数据（与消息日志同生命周期，如 pending_compact）。

        不存在返回 None。损坏数据由后端丢弃（返回 None）。
        key 必须是文件名安全的内部常量。
        """

    @abstractmethod
    def write_aux(self, key: str, data: dict[str, Any]) -> None:
        """写入辅助数据。"""

    @abstractmethod
    def delete_aux(self, key: str) -> None:
        """删除辅助数据。不存在为 NOP。"""


class SessionStore(ABC):
    """一个 session 全部持久状态的唯一所有者。

    实现：FileSessionStore（现有文件布局）、MemorySessionStore（不落盘）。
    """

    name: str = "abstract"

    @abstractmethod
    def load_metadata(self, session_id: str) -> SessionMetadata | None:
        """加载元数据。无记录返回 None。"""

    @abstractmethod
    def save_metadata(self, session_id: str, metadata: SessionMetadata) -> None:
        """保存元数据（全 None 时跳过，等价于无记录）。"""

    @abstractmethod
    def open_log(self, session_id: str) -> MessageLog:
        """打开 session 的消息日志句柄。"""

    @abstractmethod
    def list_summaries(self) -> list[SessionSummary]:
        """列举所有有消息的 session。"""

    @abstractmethod
    def resolve(self, partial: str) -> str | None:
        """模糊解析 session id（精确/通配/前缀/包含），唯一匹配返回 id，否则 None。"""

    @abstractmethod
    def exists(self, session_id: str) -> bool:
        """session 是否存在（有元数据或消息日志）。"""
