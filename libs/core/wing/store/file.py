"""
wing/store/file.py — 文件后端。

磁盘布局（与重构前完全一致，老 session 零迁移）：

    <root>/<session_id>/
    ├── metadata.json        SessionMetadata（exclude_none）
    ├── history.jsonl        append-only 消息记录（每行一条 dict + ts）
    ├── newest.json          活跃链人类可读快照（只写不读，列表标题回退除外）
    ├── <aux-key>.json       aux kv（如 pending_compact.json）
    └── subagents/           子 agent 历史（explorer 工具产物）
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

from wing.common.fs import atomic_write_json
from wing.common.logger import log
from wing.store.base import MessageLog, SessionMetadata, SessionStore, SessionSummary


class FileMessageLog(MessageLog):
    """文件消息日志：history.jsonl（append+fsync）+ newest.json（原子快照）。"""

    _HISTORY = "history.jsonl"
    _NEWEST = "newest.json"

    def __init__(self, path: Path | str) -> None:
        self._path = Path(path)

    @property
    def path(self) -> Path:
        """日志目录（文件后端特有，不属于 MessageLog 接口）。"""
        return self._path

    def _aux_path(self, key: str) -> Path:
        return self._path / f"{key}.json"

    # ── 记录 ──────────────────────────────────

    def load_all(self) -> list[dict[str, Any]]:
        hist = self._path / self._HISTORY
        if not hist.exists():
            return []
        records: list[dict[str, Any]] = []
        with open(hist, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        return records

    def append(self, records: list[dict[str, Any]]) -> None:
        if not records:
            return
        self._path.mkdir(parents=True, exist_ok=True)
        lines = [json.dumps(r, ensure_ascii=False) for r in records]
        with open(self._path / self._HISTORY, "a", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")
            f.flush()
            os.fsync(f.fileno())

    def write_snapshot(self, records: list[dict[str, Any]]) -> None:
        atomic_write_json(self._path / self._NEWEST, records)

    # ── aux kv ────────────────────────────────

    def read_aux(self, key: str) -> dict[str, Any] | None:
        path = self._aux_path(key)
        if not path.exists():
            return None
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except Exception as e:
            # 损坏的 aux 数据丢弃（与重构前 pending_compact 行为一致）
            log.warning(f"Corrupted aux '{key}' at {path}, deleting: {e}")
            self.delete_aux(key)
            return None

    def write_aux(self, key: str, data: dict[str, Any]) -> None:
        atomic_write_json(self._aux_path(key), data)

    def delete_aux(self, key: str) -> None:
        path = self._aux_path(key)
        if path.exists():
            try:
                path.unlink()
            except OSError:
                pass


class FileSessionStore(SessionStore):
    """文件后端：session 目录布局的唯一管理者。"""

    name = "file"

    _METADATA = "metadata.json"

    def __init__(self, root: Path | str) -> None:
        self._root = Path(root)

    @property
    def root(self) -> Path:
        """存储根目录（文件后端特有，不属于 SessionStore 接口）。"""
        return self._root

    def _session_dir(self, session_id: str) -> Path:
        return self._root / session_id

    # ── metadata ──────────────────────────────

    def load_metadata(self, session_id: str) -> SessionMetadata | None:
        path = self._session_dir(session_id) / self._METADATA
        if not path.exists():
            return None
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except Exception as e:
            # 不静默降级：损坏的 metadata 若被下一次 save 无痕覆盖，
            # 会丢失 forked_from/标题等字段——正是本轮要消灭的数据丢失。
            log.warning(
                f"Corrupted metadata.json for session '{session_id}' at {path}: {e}. "
                "Treating as empty; next save will overwrite it."
            )
            return SessionMetadata()
        return SessionMetadata.model_validate(data)

    def save_metadata(self, session_id: str, metadata: SessionMetadata) -> None:
        data = metadata.model_dump(exclude_none=True)
        if not data:
            return
        atomic_write_json(self._session_dir(session_id) / self._METADATA, data)

    # ── log ───────────────────────────────────

    def open_log(self, session_id: str) -> FileMessageLog:
        return FileMessageLog(self._session_dir(session_id))

    # ── 查询 ──────────────────────────────────

    def exists(self, session_id: str) -> bool:
        """精确判断 session 是否存在（有 metadata 或消息记录）。"""
        session_dir = self._session_dir(session_id)
        if not session_dir.is_dir():
            return False
        return (
            (session_dir / self._METADATA).exists()
            or (session_dir / FileMessageLog._NEWEST).exists()
            or (session_dir / FileMessageLog._HISTORY).exists()
        )

    def list_summaries(self) -> list[SessionSummary]:
        """列举有消息的 session。

        存在性判据：history.jsonl 或 newest.json 存在（history.jsonl 是 source of
        truth，newest.json 兼容崩溃窗口与老布局）。标题回退优先读 newest.json，
        其次 history.jsonl。两者均不可读的 session 跳过。
        """
        if not self._root.exists():
            return []

        result: list[SessionSummary] = []
        for session_dir in self._root.iterdir():
            if not session_dir.is_dir():
                continue
            newest = session_dir / FileMessageLog._NEWEST
            history = session_dir / FileMessageLog._HISTORY
            if not newest.exists() and not history.exists():
                continue

            metadata = self.load_metadata(session_dir.name) or SessionMetadata()
            first_user: str | None = None
            if metadata.session_name is None:
                records = self._read_snapshot(newest)
                if records is None:
                    records = self._read_history_records(history)
                if records is None:
                    continue
                for msg in records:
                    if msg.get("role") == "user":
                        first_user = (msg.get("content") or "")[:100]
                        break

            result.append(
                SessionSummary(
                    id=session_dir.name,
                    metadata=metadata,
                    first_user_message=first_user,
                )
            )
        return result

    @staticmethod
    def _read_snapshot(path: Path) -> list[dict] | None:
        """读 newest.json（dict 列表）。不存在或损坏返回 None。"""
        if not path.exists():
            return None
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except Exception:
            return None

    @staticmethod
    def _read_history_records(path: Path) -> list[dict] | None:
        """从 history.jsonl 提取记录（跳过损坏行）。不存在返回 None。"""
        if not path.exists():
            return None
        records: list[dict] = []
        try:
            for line in path.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        except OSError:
            return None
        return records
