"""
wing/store/file.py — 文件后端。

磁盘布局（事件系统变更后，newest.json 快照已移除——重放职责由
history.jsonl 的混合日志承担，遗留快照文件不读不写不删）：

    <root>/<session_id>/
    ├── metadata.json        SessionMetadata（exclude_none）
    ├── history.jsonl        append-only 混合记录（Message + 事件，每行一条 dict + ts）
    ├── <aux-key>.json       aux kv（如 pending_compact.json）
    └── subagents/           遗留：旧子 agent 历史（写入方已删除；不再写入，现有数据不读不删）

    <root>/.media/<id[:2]>/<id>   会话媒体池（内容寻址，跨 session 共享）
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Iterator

from wing.common.fs import atomic_write_bytes, atomic_write_json
from wing.common.logger import log
from wing.store.base import (
    MessageLog,
    SessionMetadata,
    SessionStore,
    SessionSummary,
    validate_media_content,
    validate_media_id,
)


class FileMessageLog(MessageLog):
    """文件消息日志：history.jsonl（append+fsync）。"""

    _HISTORY = "history.jsonl"

    def __init__(self, path: Path | str) -> None:
        self._path = Path(path)

    @property
    def path(self) -> Path:
        """日志目录（文件后端特有，不属于 MessageLog 接口）。"""
        return self._path

    def _aux_path(self, key: str) -> Path:
        return self._path / f"{key}.json"

    # ── 记录 ──────────────────────────────────

    def iter_all(self) -> Iterator[dict[str, Any]]:
        """流式读取 history.jsonl（逐行解析，不物化整份文件）。

        只产出 dict 记录：无法解析的行、以及能解析但不是 dict 的行
        （``123`` / ``"abc"``）一律跳过——契约是"记录是 dict"，放行会让
        消费方（fork 切片 / 标题回退）在 ``record.get`` 上炸掉。
        """
        hist = self._path / self._HISTORY
        if not hist.exists():
            return
        with open(hist, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(record, dict):
                    yield record

    def append(self, records: list[dict[str, Any]]) -> None:
        """追加记录：整批先序列化 + UTF-8 编码，再打开文件逐条写。

        两件事都要保：

        - **不留半批**：序列化 / 编码失败（不可序列化的值、孤立代理字符）
          必须发生在任何写入之前——否则失败的批量追加会在盘上留下半个
          前缀，而 fork 的批量前缀追加失败会留下"有历史没 metadata"的
          幽灵会话（该目录此后会被 list 到）；
        - **不拼大字符串**：fork/compact 的批量前缀动辄 MiB 级，``join``
          会把同一份数据再复制一遍。逐条写即可——崩溃窗口仍是最后一行
          可能截断（加载路径跳过损坏行）。

        代价是整批一份编码副本（``blobs``）——批量只出现在 fork / compact
        这类重塑路径上，换 all-or-nothing 划算；逐轮的小批量追加可忽略。
        """
        if not records:
            return
        blobs = [json.dumps(r, ensure_ascii=False).encode("utf-8") for r in records]
        self._path.mkdir(parents=True, exist_ok=True)
        with open(self._path / self._HISTORY, "ab") as f:
            for blob in blobs:
                f.write(blob)
                f.write(b"\n")
            f.flush()
            os.fsync(f.fileno())

    # ── aux kv ────────────────────────────────

    def read_aux(self, key: str) -> dict[str, Any] | None:
        path = self._aux_path(key)
        if not path.exists():
            return None
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except Exception as e:
            # 损坏的 aux 数据丢弃（删除后按"无值"处理）
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
    _MEDIA_DIR = ".media"

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
            # 会丢失 forked_from / 标题等字段，因此留 warning。
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

    # ── 媒体字节 ──────────────────────────────

    def _media_path(self, media_id: str) -> Path:
        """内容寻址路径：<root>/.media/<id[:2]>/<id>（两级散列，防单目录膨胀）。

        id 校验必须发生在拼接路径之前——它是文件名本身，脏 id 即路径穿越。
        """
        safe_id = validate_media_id(media_id)
        return self._root / self._MEDIA_DIR / safe_id[:2] / safe_id

    def write_media(self, media_id: str, data: bytes) -> None:
        """原子写入媒体字节（幂等：内容寻址下已存在即同内容，跳过）。

        首写路径校验 id 与字节一致（内容寻址完整性，review r1 N2）——已存在
        时跳过（对象内容在首写时已校验过，跳过省一次全量哈希）。
        """
        path = self._media_path(media_id)
        if path.exists():
            return
        validate_media_content(media_id, data)
        atomic_write_bytes(path, data)

    def read_media(self, media_id: str) -> bytes | None:
        path = self._media_path(media_id)
        if not path.exists():
            return None
        try:
            return path.read_bytes()
        except OSError as e:
            # 损坏/权限问题按"读不到"降级（序列化侧转占位文本），不打断请求。
            log.warning(f"Failed to read media '{media_id}' at {path}: {e}")
            return None

    # ── 查询 ──────────────────────────────────

    def exists(self, session_id: str) -> bool:
        """精确判断 session 是否存在（判据：metadata 或 history.jsonl）。"""
        session_dir = self._session_dir(session_id)
        if not session_dir.is_dir():
            return False
        return (session_dir / self._METADATA).exists() or (
            session_dir / FileMessageLog._HISTORY
        ).exists()

    def list_summaries(self) -> list[SessionSummary]:
        """列举有消息的 session。

        存在性判据：history.jsonl 存在（唯一事实来源日志）。标题回退从
        history.jsonl 提取第一条 user 消息；history.jsonl 不可读时仍列出
        该 session（first_user_message 为 None，上层按"无标题"处理——
        无标题的条目最终是否进列表由 SessionManager 决定）。
        遗留的 newest.json 文件不读不删（快照已废弃）。
        """
        if not self._root.exists():
            return []

        result: list[SessionSummary] = []
        for session_dir in self._root.iterdir():
            if not session_dir.is_dir():
                continue
            history = session_dir / FileMessageLog._HISTORY
            if not history.exists():
                continue

            metadata = self.load_metadata(session_dir.name) or SessionMetadata()
            first_user: str | None = None
            if metadata.session_name is None:
                first_user = self._first_user_message(session_dir.name)

            result.append(
                SessionSummary(
                    id=session_dir.name,
                    metadata=metadata,
                    first_user_message=first_user,
                )
            )
        return result

    def _first_user_message(self, session_id: str) -> str | None:
        """提取第一条 user 消息（截断 100 字符）作标题回退。

        流式：命中即停——标题只用这一条文本，不把整份历史读进内存；解析与
        "跳过损坏行"复用 ``MessageLog.iter_all``，不另写一份。日志不可读
        返回 None（上层按"无标题"处理）。
        """
        try:
            for record in self.open_log(session_id).iter_all():
                if record.get("role") == "user":
                    return (record.get("content") or "")[:100]
        except OSError:
            return None
        return None
