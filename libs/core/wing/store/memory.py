"""
wing/store/memory.py — 内存后端（不落盘）。

进程级存储：重启即消失，这是语义而非缺陷。
支撑"session 不落盘、仅本次会话期间有效"的场景。
"""

from __future__ import annotations

from typing import Any, Iterator

from wing.common.utils import validate_session_id
from wing.store.base import (
    MessageLog,
    SessionMetadata,
    SessionStore,
    SessionSummary,
    validate_media_content,
    validate_media_id,
)


class MemoryMessageLog(MessageLog):
    """进程内消息日志：list 存记录，dict 存 aux。"""

    def __init__(self) -> None:
        self._records: list[dict[str, Any]] = []
        self._aux: dict[str, dict[str, Any]] = {}

    def iter_all(self) -> Iterator[dict[str, Any]]:
        yield from self._records

    def append(self, records: list[dict[str, Any]]) -> None:
        self._records.extend(records)

    def read_aux(self, key: str) -> dict[str, Any] | None:
        return self._aux.get(key)

    def write_aux(self, key: str, data: dict[str, Any]) -> None:
        self._aux[key] = data

    def delete_aux(self, key: str) -> None:
        self._aux.pop(key, None)


class MemorySessionStore(SessionStore):
    """进程内 session 存储。"""

    name = "memory"
    durable = False
    """非持久：逐出等于数据销毁（SessionManager 逐出判定据此跳过）。"""

    def __init__(self) -> None:
        self._metadata: dict[str, SessionMetadata] = {}
        self._logs: dict[str, MemoryMessageLog] = {}
        self._media: dict[str, bytes] = {}
        """媒体池：实例级共享（同一 store 的多个会话读写同一字典，与 file
        后端"同一 root 共享 .media"语义一致）。durable=False 决定它不跨进程。"""

    # ── metadata ──────────────────────────────

    def load_metadata(self, session_id: str) -> SessionMetadata | None:
        validate_session_id(session_id)
        meta = self._metadata.get(session_id)
        return meta.model_copy() if meta is not None else None

    def save_metadata(self, session_id: str, metadata: SessionMetadata) -> None:
        validate_session_id(session_id)
        # 空数据 + 无现存记录 = 不创造空记录（与 file 后端"首次写入前不存在"
        # 语义一致）；有现存记录则照写——"清空"必须可持久化。
        if (
            not metadata.model_dump(exclude_none=True)
            and session_id not in self._metadata
        ):
            return
        self._metadata[session_id] = metadata.model_copy()

    # ── log ───────────────────────────────────

    def open_log(self, session_id: str) -> MemoryMessageLog:
        validate_session_id(session_id)
        message_log = self._logs.get(session_id)
        if message_log is None:
            message_log = MemoryMessageLog()
            self._logs[session_id] = message_log
        return message_log

    # ── 媒体字节 ──────────────────────────────

    def write_media(self, media_id: str, data: bytes) -> None:
        """写入媒体字节（幂等：内容寻址下同名即同内容，首写即终值）。

        首写路径校验 id 与字节一致（内容寻址完整性，review r1 N2）。
        """
        validate_media_id(media_id)
        if media_id in self._media:
            return
        validate_media_content(media_id, data)
        self._media[media_id] = data

    def read_media(self, media_id: str) -> bytes | None:
        validate_media_id(media_id)
        return self._media.get(media_id)

    # ── 查询 ──────────────────────────────────

    def _live_session_ids(self) -> set[str]:
        """有 metadata 或有消息的 session（对齐文件后端"首次写入前不存在"语义：
        仅 open_log 而未写入任何记录的 session 不可解析）。"""
        ids = set(self._metadata)
        for session_id, message_log in self._logs.items():
            if next(message_log.iter_all(), None) is not None:
                ids.add(session_id)
        return ids

    def exists(self, session_id: str) -> bool:
        """精确判断 session 是否存在。id 不合规即 ValueError（同 file 后端）。"""
        validate_session_id(session_id)
        return session_id in self._live_session_ids()

    def list_summaries(self) -> list[SessionSummary]:
        """列举 session（存在性判据：有日志记录 **或** 带标签的 metadata）。

        带标会话即使还没有首条消息（"创建即打标"的窗口期）也可被列表到——
        上层据此让 ``ps --tag`` / ``tag --list`` 立即找得到；是否最终进列表
        由 SessionManager 决定（无名且无标的条目会被它过滤）。

        与 file 后端"目录名不契合 session id 格式即跳过"的口径一致：memory
        后端的 key 由会话层生成，天然合规。
        """
        result: list[SessionSummary] = []
        for session_id in set(self._metadata) | set(self._logs):
            metadata = self.load_metadata(session_id) or SessionMetadata()
            message_log = self._logs.get(session_id)
            has_records = (
                message_log is not None
                and next(message_log.iter_all(), None) is not None
            )
            if not has_records and not metadata.tags:
                continue

            first_user: str | None = None
            if metadata.session_name is None and message_log is not None:
                for record in message_log.iter_all():
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
