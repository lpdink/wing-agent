"""
wing/store/memory.py — 内存后端（不落盘）。

进程级存储：重启即消失，这是语义而非缺陷。
支撑"session 不落盘、仅本次会话期间有效"的场景。
"""

from __future__ import annotations

from typing import Any

from wing.store.base import MessageLog, SessionMetadata, SessionStore, SessionSummary


class MemoryMessageLog(MessageLog):
    """进程内消息日志：list 存记录，dict 存 aux，快照 NOP。"""

    def __init__(self) -> None:
        self._records: list[dict[str, Any]] = []
        self._aux: dict[str, dict[str, Any]] = {}

    def load_all(self) -> list[dict[str, Any]]:
        return list(self._records)

    def append(self, records: list[dict[str, Any]]) -> None:
        self._records.extend(records)

    def write_snapshot(self, records: list[dict[str, Any]]) -> None:
        pass  # 无人类可读快照需求

    def read_aux(self, key: str) -> dict[str, Any] | None:
        return self._aux.get(key)

    def write_aux(self, key: str, data: dict[str, Any]) -> None:
        self._aux[key] = data

    def delete_aux(self, key: str) -> None:
        self._aux.pop(key, None)


class MemorySessionStore(SessionStore):
    """进程内 session 存储。"""

    name = "memory"

    def __init__(self) -> None:
        self._metadata: dict[str, SessionMetadata] = {}
        self._logs: dict[str, MemoryMessageLog] = {}

    # ── metadata ──────────────────────────────

    def load_metadata(self, session_id: str) -> SessionMetadata | None:
        meta = self._metadata.get(session_id)
        return meta.model_copy() if meta is not None else None

    def save_metadata(self, session_id: str, metadata: SessionMetadata) -> None:
        if not metadata.model_dump(exclude_none=True):
            return
        self._metadata[session_id] = metadata.model_copy()

    # ── log ───────────────────────────────────

    def open_log(self, session_id: str) -> MemoryMessageLog:
        message_log = self._logs.get(session_id)
        if message_log is None:
            message_log = MemoryMessageLog()
            self._logs[session_id] = message_log
        return message_log

    # ── 查询 ──────────────────────────────────

    def _live_session_ids(self) -> set[str]:
        """有 metadata 或有消息的 session（对齐文件后端"首次写入前不存在"语义：
        仅 open_log 而未写入任何记录的 session 不可解析）。"""
        ids = set(self._metadata)
        for session_id, message_log in self._logs.items():
            if message_log.load_all():
                ids.add(session_id)
        return ids

    def exists(self, session_id: str) -> bool:
        """精确判断 session 是否存在。"""
        return session_id in self._live_session_ids()

    def list_summaries(self) -> list[SessionSummary]:
        """列举有消息的 session（与文件后端 newest.json 语义对齐）。"""
        result: list[SessionSummary] = []
        for session_id in set(self._metadata) | set(self._logs):
            message_log = self._logs.get(session_id)
            records = message_log.load_all() if message_log else []
            if not records:
                continue

            metadata = self.load_metadata(session_id) or SessionMetadata()
            first_user: str | None = None
            if metadata.session_name is None:
                for record in records:
                    if record.get("role") == "user":
                        first_user = (record.get("content") or "")[:100]
                        break

            result.append(
                SessionSummary(
                    id=session_id,
                    metadata=metadata,
                    first_user_message=first_user,
                )
            )
        return result
