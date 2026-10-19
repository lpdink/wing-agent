"""wing/db/database.py — 进程级 SQLite 门面（连接 + pragma + 迁移 + 降级）。

不属于 ``wing/store/``：那里是**会话**持久状态的唯一所有者（file / memory 后端）；
这里是进程级的旁路库（审计行、唤醒触发器），与会话文件互不读写。

**降级是一等公民**：打开 / 迁移失败只留一行 ERROR 日志，此后本对象的所有写入静默
失败（返回 ``False``）、查询返回空列表，异常绝不逃出——审计与唤醒都不能把 agent
带崩，DB 打不开时网关必须照常工作。

**锁冲突不是降级理由**：把库转成 WAL 需要排他访问，多个进程同一刻首次打开同一个
新库时，输家会拿到 ``database is locked``。那是良性竞态（赢家毫秒级就迁移完），
整段打开因此对锁类错误重试几次；只有重试耗尽才降级。转换期的 ``busy_timeout``
故意设得很短——否则一次竞态会在事件循环线程上冻满 5 秒。

连接归属：单连接、只在事件循环线程使用（``check_same_thread`` 保持默认 ``True``）；
写入一律同步 + 自动提交（``isolation_level=None``），单条 INSERT 微秒级，不引入写
线程、不做批处理。
"""

from __future__ import annotations

import sqlite3
import time
from collections.abc import Iterable, Sequence
from pathlib import Path
from typing import Any

from wing.common.logger import log
from wing.db.migrations import MIGRATIONS, SCHEMA_VERSION

#: 打开重试次数（含首次）与退避基数。只在**锁类**错误上重试。
_OPEN_ATTEMPTS = 5
_OPEN_RETRY_DELAY_S = 0.05

#: 打开期（转 WAL + 迁移）的等锁上限；稳态值是 5000（见下）。
_OPEN_BUSY_TIMEOUT_MS = 200

#: 整个打开过程的总时长上界（秒）。到点即降级——它跑在事件循环线程上。
_OPEN_DEADLINE_S = 2.0

#: 稳态写等锁上限（契约值：5000）。
_BUSY_TIMEOUT_MS = 5000


class SchemaVersionTooNew(RuntimeError):
    """磁盘上的 ``user_version`` 高于本代码认识的版本——不猜、不降级写。"""


def _user_version(conn: sqlite3.Connection) -> int:
    return int(conn.execute("PRAGMA user_version").fetchone()[0])


def _is_lock_error(exc: BaseException) -> bool:
    """是否锁类错误（SQLITE_BUSY / SQLITE_LOCKED 都被 sqlite3 报成 OperationalError）。"""
    if not isinstance(exc, sqlite3.OperationalError):
        return False
    message = str(exc).lower()
    return "locked" in message or "busy" in message


def split_statements(script: str) -> list[str]:
    """把多语句 SQL 脚本切成单条语句（迁移要逐条 ``execute`` 才能在事务里跑）。

    ``executescript`` 不能用：它在自动提交模式下逐条落盘，做不到「全有或全无」；
    事务必须由调用方显式 ``BEGIN`` / ``COMMIT``。切分按 SQLite 自己的判定
    （``sqlite3.complete_statement``）——注释、字符串字面量、分号都被它正确跳过。
    """
    statements: list[str] = []
    buffer = ""
    for line in script.splitlines(keepends=True):
        buffer += line
        if sqlite3.complete_statement(buffer):
            statements.append(buffer)
            buffer = ""
    if buffer.strip():
        statements.append(buffer)  # 不完整语句：原样交给 SQLite 报错
    return statements


class Database:
    """进程级 SQLite 门面。构造即尝试打开；失败则永久降级（``ready=False``）。"""

    def __init__(self, path: str | Path) -> None:
        self._path = Path(path)
        self._conn: sqlite3.Connection | None = None
        self._ready = False
        self._warned_write_error = False
        self._open()

    # ── 状态 ─────────────────────────────────────

    @property
    def ready(self) -> bool:
        """打开成功且迁移成功。``False`` 之后的一切读写都是 no-op。"""
        return self._ready

    @property
    def path(self) -> Path:
        return self._path

    # ── 打开 + 迁移 ───────────────────────────────

    def _open(self) -> None:
        """建目录 → 连接 → pragma → 迁移。锁类错误重试，其余立即降级。

        总时长有上界（``_OPEN_DEADLINE_S``）：这个调用点在事件循环线程上（懒打开
        发生在 ``EventBus.emit`` 里），最坏情况也必须是一秒级而不是几十秒。
        """
        deadline = time.monotonic() + _OPEN_DEADLINE_S
        for attempt in range(1, _OPEN_ATTEMPTS + 1):
            try:
                self._open_once()
            except SchemaVersionTooNew as exc:
                self._degrade(exc)
                return
            except Exception as exc:
                self._release_connection()
                if (
                    attempt == _OPEN_ATTEMPTS
                    or not _is_lock_error(exc)
                    or time.monotonic() >= deadline
                ):
                    self._degrade(exc)
                    return
                time.sleep(_OPEN_RETRY_DELAY_S * attempt)
                continue
            self._ready = True
            return

    def _open_once(self) -> None:
        self._path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(self._path, isolation_level=None)
        conn.row_factory = sqlite3.Row
        self._conn = conn
        # 打开期（转 WAL + 迁移）的等锁上限故意很短：冲突交给外层重试，稳态的
        # 写等锁在成功后设回 5s。否则一次良性竞态会在事件循环线程上冻满 5 秒。
        conn.execute(f"PRAGMA busy_timeout={_OPEN_BUSY_TIMEOUT_MS}")
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        self._migrate(conn)
        conn.execute(f"PRAGMA busy_timeout={_BUSY_TIMEOUT_MS}")

    def _migrate(self, conn: sqlite3.Connection) -> None:
        """按 ``MIGRATIONS`` 升级到 ``SCHEMA_VERSION``（已是最新则 no-op）。

        每条迁移连同 ``PRAGMA user_version`` 在同一事务里：半截 schema 会让下一次
        启动重跑同一迁移并再次失败（永久降级），所以失败必须整体回滚。
        """
        current = _user_version(conn)
        if current > SCHEMA_VERSION:
            raise SchemaVersionTooNew(
                f"user_version={current} > known {SCHEMA_VERSION}"
                "（库由更新版本的代码写过；本进程不写不迁）"
            )
        for version, script in MIGRATIONS:
            if version <= current:
                continue
            conn.execute("BEGIN")
            try:
                # 版本判定必须发生在**拿到写锁之后**：并发首开时另一个进程可能刚把
                # 这一版迁完并提交，此刻本次事务的快照仍是旧的——照旧跑 DDL 会撞
                # "table … already exists"（不是锁类错误，一次就把库判死）。重读即
                # 跳过；若快照已过期，SQLite 会报 database is locked，落进外层重试。
                if _user_version(conn) >= version:
                    conn.execute("COMMIT")
                    continue
                for statement in split_statements(script):
                    conn.execute(statement)
                conn.execute(f"PRAGMA user_version = {version}")
                conn.execute("COMMIT")
            except Exception:
                if conn.in_transaction:
                    conn.execute("ROLLBACK")
                raise

    def _degrade(self, exc: BaseException) -> None:
        """关掉连接并留一行 ERROR；之后不再有任何日志。"""
        self._release_connection()
        log.error(f"wing.db disabled at {self._path}: {exc!r}")

    def _release_connection(self) -> None:
        self._ready = False
        conn, self._conn = self._conn, None
        if conn is not None:
            try:
                conn.close()
            except Exception:
                pass

    # ── 读 / 写 ───────────────────────────────────

    def execute(self, sql: str, params: Sequence[Any] = ()) -> bool:
        """执行一条写语句。返回是否真正落库；任何异常都被吞掉（返回 ``False``）。"""
        if not self._ready or self._conn is None:
            return False
        try:
            self._conn.execute(sql, params)
        except Exception as exc:
            self._warn_write_error(exc)
            return False
        return True

    def executemany(self, sql: str, rows: Iterable[Sequence[Any]]) -> bool:
        """执行一条写语句（多组参数）；语义同 :meth:`execute`。"""
        if not self._ready or self._conn is None:
            return False
        try:
            self._conn.executemany(sql, rows)
        except Exception as exc:
            self._warn_write_error(exc)
            return False
        return True

    def query(self, sql: str, params: Sequence[Any] = ()) -> list[sqlite3.Row]:
        """执行查询。降级 / 出错一律返回空列表（不抛、不刷日志）。"""
        if not self._ready or self._conn is None:
            return []
        try:
            return self._conn.execute(sql, params).fetchall()
        except Exception:
            return []

    def close(self) -> None:
        """关闭连接（幂等；降级过的对象是 no-op）。"""
        self._ready = False
        conn, self._conn = self._conn, None
        if conn is None:
            return
        try:
            conn.close()
        except Exception:
            pass

    # ── 内部 ─────────────────────────────────────

    def _warn_write_error(self, exc: BaseException) -> None:
        """首次写失败留一条 WARNING（正在丢数据），此后闭嘴——审计是 best-effort。"""
        if self._warned_write_error:
            return
        self._warned_write_error = True
        log.warning(
            f"wing.db write failed at {self._path}（后续写入静默丢弃）: {exc!r}"
        )
