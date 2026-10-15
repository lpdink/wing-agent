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
from wing.common.utils import is_valid_session_id, validate_session_id
from wing.store.base import (
    MessageLog,
    SessionMetadata,
    SessionStore,
    SessionSummary,
    validate_media_content,
    validate_media_id,
)


def _is_same_directory(candidate: Path, target: Path) -> bool:
    """两个路径是否指向同一个目录（同 inode）；任一侧不可达即 False。

    仅用于探测文件系统别名（大小写 / Unicode 归一化不敏感）：请求路径解析成功
    但目录项名字对不上时，逐个目录项与它比对。
    """
    try:
        return os.path.samefile(candidate, target)
    except OSError:
        return False


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
        """会话目录。**所有文件路径拼接的唯一入口**。

        拼接前先过 session id 闸门（``validate_session_id``）：id 是路径
        组件，不合规的值（``../`` / 绝对路径 / 控制字符 / 点开头 / 超长）
        必须在此失败——这是防路径穿越的最终防线（会话层的解析闸门是第一道；
        两道都过才可能触碰文件系统）。id 由会话层确定（默认后端自生成，
        编排方可经 create-or-adopt 指定），闸门只防穿越与卫生。
        """
        safe_id = validate_session_id(session_id)
        return self._root / safe_id

    # ── metadata ──────────────────────────────

    def load_metadata(self, session_id: str) -> SessionMetadata | None:
        path = self._session_dir(session_id) / self._METADATA
        if not path.exists():
            return None
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            return SessionMetadata.model_validate(data)
        except Exception as e:
            # 损坏的 metadata（JSON 解析失败 / schema 不符）统一降级：
            # warning + 空对象。不抛——损坏数据不能把读取路径打挂（列表 /
            # TUI / 水合；一个坏目录不该噎死整个会话列表）；也不静默——
            # 下一次 save 会把它覆盖成当前状态（丢失的字段因此可见于日志）。
            log.warning(
                f"Corrupted metadata.json for session '{session_id}' at {path}: {e}. "
                "Treating as empty; next save will overwrite it."
            )
            return SessionMetadata()

    def save_metadata(self, session_id: str, metadata: SessionMetadata) -> None:
        data = metadata.model_dump(exclude_none=True)
        path = self._session_dir(session_id) / self._METADATA
        # 空数据 + 无现存文件 = 不创造空记录（首次写入语义）；有现存文件则
        # 照写——"清空"必须可持久化（如移除最后一个标签后 metadata 变空，
        # 静默跳过会让删除在下次读取时"复活"）。
        if not data and not path.exists():
            return
        atomic_write_json(path, data)

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

        首写路径校验 id 与字节一致（内容寻址完整性）——已存在
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

    def resolve_stored_id(self, session_id: str) -> str | None:
        """该 id 在磁盘上对应的**目录名**；目录不存在返回 None。

        不能只 stat 请求路径：文件系统可能对**大小写**（macOS APFS 默认 /
        Windows NTFS）或 **Unicode 归一化**不敏感——``team-a`` 会解析到
        ``Team-A`` 的目录，名字却与请求值不同。只认请求字符串会让两个内存会话
        共用一份 history.jsonl（静默数据混合），因此这里**逐字比对目录项**并返回
        真实名字。大小写敏感的 FS（Linux ext4/btrfs）上没有别名，逐字命中即返回。

        判据是**目录存在**（不要求"是会话"）：目录已存在但还没有 metadata /
        history（失败的 create 残留、手工目录）时，新建也必须沿用真实目录名，
        否则同一个目录会被两个 id 写。
        """
        safe_id = validate_session_id(session_id)
        real = self._on_disk_name(self._root / safe_id)
        # 别名名同样要过闸门（否则会给内存索引塞进一个闸门拒绝的键）
        return real if real is not None and is_valid_session_id(real) else None

    def claim_session_id(self, session_id: str) -> str:
        """新建会话的键认领：``mkdir`` 的成败把"别名"变成确定事实（见接口说明）。

        - 目录已在（逐字或别名）→ **真实目录名**（别名在 mkdir 之前就被看见，
          不必靠 EEXIST 反推）；
        - 目录不在 → ``mkdir``：成功 ⇒ 这个名字在本文件系统上还是空闲的（Linux 的
          敏感 FS 上两个大小写变体是两个目录），逐字使用；``FileExistsError`` ⇒
          路径被别的东西占住（同名文件 / 竞态）→ 尽力取真实名字；
        - 其它 ``OSError``（只读挂载 / 权限）→ 退回逐字使用，让真正的写路径去报错
          （不把 create 打得过早死掉，错误面收在写盘处）。

        建出来的空目录是"会话正在被创建"的正常前奏：空目录不算会话（见
        ``exists``），但**必须存在**——后面来的大小写变体正是靠它才看得见别名。
        """
        safe_id = validate_session_id(session_id)
        existing = self.resolve_stored_id(safe_id)
        if existing is not None:
            return existing
        target = self._root / safe_id
        try:
            target.mkdir(parents=True, exist_ok=False)
        except FileExistsError:
            real = self._on_disk_name(target)
            return real if real is not None and is_valid_session_id(real) else safe_id
        except OSError as e:
            log.warning(f"session dir '{target}' not created here: {e}")
            return safe_id
        return safe_id

    def _on_disk_name(self, target: Path) -> str | None:
        """（内部）``target`` 解析到的**真实目录名**，逐字优先；不是目录返回 None。

        逐字命中是常见情形（大小写敏感的 FS 上唯一可能的情形），也是唯一不必
        探测别名的情形；逐字目录项不在时用 ``samefile`` 找别名——别名即"请求路径
        能被解析、但磁盘上的名字是另一个"（大小写 / Unicode 归一化不敏感）。
        """
        if not target.is_dir():
            return None
        alias: str | None = None
        for entry in self._root.iterdir():
            if entry.name == target.name:
                return target.name
            if alias is None and _is_same_directory(entry, target):
                alias = entry.name
        return alias

    def list_summaries(self) -> list[SessionSummary]:
        """列举 session（存在性判据：history.jsonl **或** 带标签的 metadata）。

        带标会话即使还没有首条消息（"创建即打标"的窗口期）也可被列表到——
        上层据此让 ``ps --tag`` / ``tag --list`` 立即找得到；是否最终进列表
        由 SessionManager 决定（无名且无标的条目会被它过滤）。

        目录名不过 session id 闸门的一律跳过：它们不是会话（也无法经任何
        API 寻址——解析闸门同样拒绝），可能只是 root 里的杂物，或是存储
        自己的点命名空间（``.media`` 媒体池）。闸门只防穿越与卫生，因此
        "点开头 / 含 ``..`` / 控制字符 / 超长"之外的名字都可能是合法会话
        （编排方可自带 UUID 等任意 id）。
        标题回退从 history.jsonl 提取第一条 user 消息；history.jsonl 不可读
        时仍列出该 session（first_user_message 为 None）。
        遗留的 newest.json 文件不读不删（快照已废弃）。
        """
        if not self._root.exists():
            return []

        result: list[SessionSummary] = []
        for session_dir in self._root.iterdir():
            if not session_dir.is_dir():
                continue
            if not is_valid_session_id(session_dir.name):
                continue
            metadata = self.load_metadata(session_dir.name) or SessionMetadata()
            history = session_dir / FileMessageLog._HISTORY
            if not history.exists() and not metadata.tags:
                continue

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
