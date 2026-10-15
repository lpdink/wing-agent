"""
wing/store/file.py — 文件后端。

磁盘布局（事件系统变更后，newest.json 快照已移除——重放职责由
history.jsonl 的混合日志承担，遗留快照文件不读不写不删）：

    <root>/<session_id>/
    ├── metadata.json        SessionMetadata（exclude_none）
    ├── history.jsonl        append-only 混合记录（Message + 事件，每行一条 dict + ts）
    ├── media/<sha256>       session 私有媒体对象（内容寻址；会话自包含的关键）
    ├── <aux-key>.json       aux kv（如 pending_compact.json）
    └── subagents/           遗留：旧子 agent 历史（写入方已删除；不再写入，现有数据不读不删）

    <root>/.archive/         归档区（点前缀 = 存储保留命名空间，不可寻址成会话）
    ├── index.json           归档索引（条目 = 归档时刻的事实快照；列表的唯一读取口）
    └── <session_id>/        被归档的会话目录（结构与 live 区一致，字节不变）

    <root>/.media/<id[:2]>/<id>   遗留共享媒体池（**只读**：老会话的引用仍解析
                                  到这里；读 miss 时物化进会话目录。不删、不写）
"""

from __future__ import annotations

import json
import os
from datetime import datetime
from pathlib import Path
from typing import Any, Iterator

from wing.common.fs import atomic_write_bytes, atomic_write_json
from wing.common.logger import log
from wing.common.utils import is_valid_session_id, validate_session_id
from wing.store.base import (
    ArchivedSession,
    MessageLog,
    SessionMetadata,
    SessionStore,
    SessionSummary,
    first_user_title,
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
    archivable = True

    _METADATA = "metadata.json"
    _MEDIA_DIR = "media"
    _LEGACY_MEDIA_DIR = ".media"
    _ARCHIVE_DIR = ".archive"
    _ARCHIVE_INDEX = "index.json"

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
        return self._load_metadata_at(self._session_dir(session_id), session_id)

    def _load_metadata_at(
        self, session_dir: Path, session_id: str
    ) -> SessionMetadata | None:
        """按**目录**读取 metadata（live / 归档区共用；session_id 只用于日志）。"""
        path = session_dir / self._METADATA
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

    # ── 媒体字节（内容寻址，session 私有）─────────

    def _media_path(self, session_id: str, media_id: str) -> Path:
        """会话私有媒体对象路径：<root>/<sid>/media/<id>。

        两道闸门都在拼接路径之前——session id 与 media id 都是路径组件，
        脏值即路径穿越（``_session_dir`` 先过 ``validate_session_id``，
        ``validate_media_id`` 校验 id 本身）。
        """
        return (
            self._session_dir(session_id)
            / self._MEDIA_DIR
            / validate_media_id(media_id)
        )

    def _legacy_media_path(self, media_id: str) -> Path:
        """遗留共享池路径：<root>/.media/<id[:2]>/<id>（只读兼容，永不写入）。

        id 校验必须发生在拼接路径之前——它是文件名本身，脏 id 即路径穿越。
        """
        safe_id = validate_media_id(media_id)
        return self._root / self._LEGACY_MEDIA_DIR / safe_id[:2] / safe_id

    def write_media(self, session_id: str, media_id: str, data: bytes) -> None:
        """原子写入会话媒体目录（幂等：内容寻址下已存在即同内容，跳过）。

        首写路径校验 id 与字节一致（内容寻址完整性）——已存在
        时跳过（对象内容在首写时已校验过，跳过省一次全量哈希）。
        """
        path = self._media_path(session_id, media_id)
        if path.exists():
            return
        validate_media_content(media_id, data)
        atomic_write_bytes(path, data)

    def read_media(self, session_id: str, media_id: str) -> bytes | None:
        """读取媒体字节：会话目录优先，miss 回退老池并物化（best-effort）。"""
        path = self._media_path(session_id, media_id)
        if path.exists():
            try:
                return path.read_bytes()
            except OSError as e:
                # 损坏/权限问题按"读不到"降级（序列化侧转占位文本），不打断请求。
                log.warning(f"Failed to read media '{media_id}' at {path}: {e}")
                return None
        return self._read_legacy_and_materialize(session_id, media_id, path)

    def media_exists(self, session_id: str, media_id: str) -> bool:
        """对象是否可寻址（会话目录或老池任一存在）。纯查询，不物化。"""
        return (
            self._media_path(session_id, media_id).exists()
            or self._legacy_media_path(media_id).exists()
        )

    def media_path(self, session_id: str, media_id: str) -> str | None:
        """对象的本地绝对路径（会话目录优先、老池兜底）；不可寻址为 None。

        纯查询（不物化）：物化时机统一归加载路径与读路径。
        """
        path = self._media_path(session_id, media_id)
        if path.exists():
            return str(path)
        legacy = self._legacy_media_path(media_id)
        return str(legacy) if legacy.exists() else None

    def materialize(self, session_id: str, media_id: str) -> bool:
        """把老池对象幂等复制进会话目录（见 SessionStore.materialize）。"""
        target = self._media_path(session_id, media_id)
        if target.exists():
            return True  # 快路径：对象已在会话目录，不碰老池
        legacy = self._legacy_media_path(media_id)
        if not legacy.exists():
            return False
        try:
            data = legacy.read_bytes()
        except OSError as e:
            log.warning(f"Failed to read media '{media_id}' at {legacy}: {e}")
            return False
        return self._materialize_into(session_id, media_id, target, data)

    def _read_legacy_and_materialize(
        self, session_id: str, media_id: str, target: Path
    ) -> bytes | None:
        """（内部）读路径的老池回退：读字节 + best-effort 物化；两处都缺返回 None。

        物化失败只 WARN——本次读取照常返回字节（序列化不该因复制失败而降级），
        下一次读取自然重试（幂等）。
        """
        legacy = self._legacy_media_path(media_id)
        if not legacy.exists():
            return None
        try:
            data = legacy.read_bytes()
        except OSError as e:
            log.warning(f"Failed to read media '{media_id}' at {legacy}: {e}")
            return None
        self._materialize_into(session_id, media_id, target, data)
        return data

    def _materialize_into(
        self, session_id: str, media_id: str, target: Path, data: bytes
    ) -> bool:
        """（内部）把字节原子写进会话媒体目录（失败 WARN + False，不抛）。

        不做内容校验：老池对象在首写时已校验（``validate_media_content``），
        物化只保证"读回什么就存什么"——与读路径的字节等价是迁移的正确性要求。
        """
        try:
            atomic_write_bytes(target, data)
            return True
        except OSError as e:
            log.warning(
                f"Failed to materialize media '{media_id}' into session "
                f"'{session_id}' at {target}: {e} (legacy copy remains readable)"
            )
            return False

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
        real = self._on_disk_name(self._root / safe_id, self._root)
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
            real = self._on_disk_name(target, self._root)
            return real if real is not None and is_valid_session_id(real) else safe_id
        except OSError as e:
            log.warning(f"session dir '{target}' not created here: {e}")
            return safe_id
        return safe_id

    def _on_disk_name(self, target: Path, parent: Path) -> str | None:
        """（内部）``target`` 在 ``parent`` 下解析到的**真实目录名**，逐字优先；
        不是目录返回 None。

        逐字命中是常见情形（大小写敏感的 FS 上唯一可能的情形），也是唯一不必
        探测别名的情形；逐字目录项不在时用 ``samefile`` 找别名——别名即"请求路径
        能被解析、但磁盘上的名字是另一个"（大小写 / Unicode 归一化不敏感）。
        live 区与归档区共用本函数（各自传自己的父目录）。
        """
        if not target.is_dir():
            return None
        alias: str | None = None
        for entry in parent.iterdir():
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
        自己的点命名空间（``.media`` 遗留媒体池）。闸门只防穿越与卫生，因此
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
        """会话标题回退素材（截断 100 字符）；规则见 ``first_user_title``。

        流式：命中即停——标题只用这一条文本，不把整份历史读进内存；解析与
        "跳过损坏行"复用 ``MessageLog.iter_all``，不另写一份。日志不可读
        返回 None（上层按"无标题"处理）。
        """
        try:
            return first_user_title(self.open_log(session_id).iter_all())
        except OSError:
            return None

    def _first_user_message_at(self, session_dir: Path) -> str | None:
        """按**目录**取标题回退素材（归档区路径用；live 区走 ``open_log``）。"""
        try:
            return first_user_title(FileMessageLog(session_dir).iter_all())
        except OSError:
            return None

    # ── 归档区 ────────────────────────────────

    def _archive_root(self) -> Path:
        return self._root / self._ARCHIVE_DIR

    def _archive_index_path(self) -> Path:
        return self._archive_root() / self._ARCHIVE_INDEX

    def resolve_archived_id(self, session_id: str) -> str | None:
        """该 id 在归档区里对应的目录名；不在归档区返回 None。

        判据与 :meth:`resolve_stored_id` 逐字对齐（目录存在即算、别名逐字比对、
        真实名过闸门），作用域换成归档区。
        """
        safe_id = validate_session_id(session_id)
        root = self._archive_root()
        real = self._on_disk_name(root / safe_id, root)
        return real if real is not None and is_valid_session_id(real) else None

    def archive(self, session_id: str) -> None:
        """移入归档区 + 登记索引（见基类契约；rename 与索引更新同一次调用）。"""
        safe_id = validate_session_id(session_id)
        live = self._root / safe_id
        if not live.is_dir():
            raise FileNotFoundError(f"session dir not found: {live}")
        target = self._archive_root() / safe_id
        if target.exists():
            raise RuntimeError(
                f"session '{safe_id}' exists in both live and archive dirs"
            )
        entry = self._archive_entry_at(
            safe_id, live, archived_at=datetime.now().isoformat()
        )
        target.parent.mkdir(parents=True, exist_ok=True)
        os.rename(live, target)
        self._upsert_archive_index(entry)

    def unarchive(self, session_id: str) -> None:
        """移回 live 区 + 删除索引条目（索引失败不回滚移动——目录是事实源）。"""
        safe_id = validate_session_id(session_id)
        source = self._archive_root() / safe_id
        if not source.is_dir():
            raise FileNotFoundError(f"archived session dir not found: {source}")
        target = self._root / safe_id
        if target.exists():
            raise RuntimeError(
                f"session '{safe_id}' exists in both live and archive dirs"
            )
        os.rename(source, target)
        self._remove_from_archive_index(safe_id)

    def list_archived(self) -> list[ArchivedSession]:
        """列出归档条目：读索引 + 与目录**双向对账**。

        - 索引缺失 / 损坏 → 按归档目录整体重建；
        - 索引里目录已不存在的条目（手工删除）→ 不投影并剔除；
        - 目录存在但索引未登记（手工拷入；或"rename 成功 + 索引写失败 /
          进程恰好死在这里"的窗口）→ 当场收编（读该目录的 metadata 建条目）。

        目录是事实源，索引只是读取加速——只信索引会让上面第二类会话永远
        列不出来。对账成本 = 每个条目一次解析 + 一次 readdir（收编才读
        metadata），低频路径上可接受。
        """
        entries = self._read_archive_index()
        if entries is None:
            entries = self._scan_archive_entries()
            # 空结果不落盘：读取路径不该创造存储痕迹（没有归档区、没有条目时，
            # 一次列表调用不该无中生有一个 .archive/index.json）。
            if entries:
                self._write_archive_index(entries)
            return entries

        alive: list[ArchivedSession] = []
        known: set[str] = set()
        for entry in entries:
            try:
                real = self.resolve_archived_id(entry.id)
            except ValueError:
                # 索引被手改出脏 id：不投影（也无法寻址）
                log.warning(f"Archive index holds an invalid session id: {entry.id!r}")
                continue
            if real is None:
                continue  # 目录已消失 → 剔除（下面统一落盘自愈）
            alive.append(entry)
            known.add(real)

        adopted = self._adopt_untracked_entries(known)
        if adopted or len(alive) != len(entries):
            if len(alive) != len(entries):
                log.info(
                    f"Archive index: dropped {len(entries) - len(alive)} stale entry(ies)"
                )
            self._write_archive_index(alive + adopted)
        return alive + adopted

    def _adopt_untracked_entries(self, known: set[str]) -> list[ArchivedSession]:
        """（内部）收编归档区里索引未登记的目录（按已知真名去重；只读）。"""
        root = self._archive_root()
        if not root.is_dir():
            return []
        adopted: list[ArchivedSession] = []
        for session_dir in root.iterdir():
            if not session_dir.is_dir() or not is_valid_session_id(session_dir.name):
                continue
            if session_dir.name in known:
                continue
            metadata = self._load_metadata_at(session_dir, session_dir.name)
            if not self._looks_like_session(session_dir, metadata):
                continue
            log.info(f"Archive index: adopting untracked dir '{session_dir.name}'")
            adopted.append(
                self._archive_entry_at(
                    session_dir.name, session_dir, archived_at=None, metadata=metadata
                )
            )
        return adopted

    def _looks_like_session(
        self, session_dir: Path, metadata: SessionMetadata | None
    ) -> bool:
        """归档区「是会话」的判据（与 live 区 ``list_summaries`` 同一口径）：
        有 ``history.jsonl`` 或带标签的 metadata。

        收编（索引在）与重建（索引缺失）共用本判据——否则同一个盘上"两次列表
        结果不一样"（索引在时收编、索引重建后又消失）。
        """
        if metadata is not None and metadata.tags:
            return True
        return (session_dir / FileMessageLog._HISTORY).exists()

    # ── 归档索引（内部）───────────────────────

    def _read_archive_index(self) -> list[ArchivedSession] | None:
        """读索引；文件不存在 → None（= 需要重建）；整份不可解析 → None + WARN。

        单条条目损坏逐条丢弃（不让一条坏记录打挂整个归档列表）。
        """
        path = self._archive_index_path()
        if not path.exists():
            return None
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except Exception as e:
            log.warning(f"Corrupted archive index at {path}: {e}; rebuilding")
            return None
        if not isinstance(data, dict) or not isinstance(data.get("sessions"), list):
            log.warning(f"Malformed archive index at {path}; rebuilding")
            return None
        entries: list[ArchivedSession] = []
        for raw in data["sessions"]:
            try:
                entries.append(ArchivedSession.model_validate(raw))
            except Exception:
                continue
        return entries

    def _write_archive_index(self, entries: list[ArchivedSession]) -> None:
        """落盘索引（原子写）；失败只 WARN——目录是事实源，索引可由重建恢复。"""
        payload = {"sessions": [e.model_dump(exclude_none=True) for e in entries]}
        try:
            self._archive_index_path().parent.mkdir(parents=True, exist_ok=True)
            atomic_write_json(self._archive_index_path(), payload)
        except OSError as e:
            log.warning(f"Failed to write archive index: {e}")

    def _upsert_archive_index(self, entry: ArchivedSession) -> None:
        """登记一条归档条目（同 id 替换）；索引不可读时先重建再写入。"""
        current = self._read_archive_index()
        if current is None:
            current = self._scan_archive_entries()
        self._write_archive_index([e for e in current if e.id != entry.id] + [entry])

    def _remove_from_archive_index(self, session_id: str) -> None:
        """删除一条归档条目（条目不存在为 no-op）。"""
        current = self._read_archive_index()
        if current is None:
            return
        pruned = [e for e in current if e.id != session_id]
        if len(pruned) != len(current):
            self._write_archive_index(pruned)

    def _scan_archive_entries(self) -> list[ArchivedSession]:
        """从归档目录反推条目（重建素材；只读，不写索引）。

        ``archived_at`` 留空：重建不知道当时的归档时刻，不回填、不发明数字。
        """
        root = self._archive_root()
        if not root.is_dir():
            return []
        entries: list[ArchivedSession] = []
        for session_dir in root.iterdir():
            if not session_dir.is_dir() or not is_valid_session_id(session_dir.name):
                continue
            metadata = self._load_metadata_at(session_dir, session_dir.name)
            if not self._looks_like_session(session_dir, metadata):
                continue
            entries.append(
                self._archive_entry_at(
                    session_dir.name, session_dir, archived_at=None, metadata=metadata
                )
            )
        return entries

    def _archive_entry_at(
        self,
        session_id: str,
        session_dir: Path,
        *,
        archived_at: str | None,
        metadata: SessionMetadata | None = None,
    ) -> ArchivedSession:
        """归档条目 = 归档时刻的事实快照（metadata + 标题回退）。"""
        if metadata is None:
            metadata = self._load_metadata_at(session_dir, session_id)
        metadata = metadata or SessionMetadata()
        return ArchivedSession(
            id=session_id,
            name=metadata.session_name or self._first_user_message_at(session_dir),
            workspace=metadata.workspace,
            template_name=metadata.template_name,
            last_interaction=metadata.last_interaction,
            archived_at=archived_at,
            tags=list(metadata.tags or []),
            model_id=metadata.model_id,
            model_name=metadata.model_name,
            provider_name=metadata.provider_name,
        )
