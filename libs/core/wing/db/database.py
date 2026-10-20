"""wing/db/database.py — 进程级 SQLite 门面（连接 + pragma + 迁移 + 降级）。

不属于 ``wing/store/``：那里是**会话**持久状态的唯一所有者（file / memory 后端）；
这里是进程级的旁路库（审计行、唤醒触发器、缓存账本），与会话文件互不读写。

**降级是一等公民**：打开 / 迁移失败只留一行 ERROR 日志，此后本对象的所有写入静默
失败（返回 ``False``）、查询返回空列表，异常绝不逃出——审计与唤醒都不能把 agent
带崩，DB 打不开时网关必须照常工作。

**并发首开不是降级理由**：把库转成 WAL 需要排他访问、迁移事务也可能被赢家挡住，
多个进程同一刻首次打开同一个新库时输家会拿到 ``database is locked``。那是良性竞态
（赢家毫秒级就迁移完），整段打开因此对锁类错误重试到总时限（``_OPEN_DEADLINE_S``）。
打开期（转 WAL + 迁移）的 ``busy_timeout`` 故意设得很短——否则一次竞态会在事件循环
线程上冻满 5 秒；迁移成功后设回稳态值。

连接归属：**进程级单连接，跨线程共享**（``check_same_thread=False``）。使用方不属于
同一个线程：审计行在事件循环线程上写，缓存账本在 ``asyncio.to_thread`` 的工作线程里
读写。所有入口因此都在一把可重入锁内（``_lock``）——单条语句天然原子，
``last_insert_rowid()`` 这类「写入 + 读回」必须走 :meth:`Database.insert`，多条语句的
原子性走 :meth:`Database.transaction`。

代价要记住：写是**同步**的，持锁期间别的线程（包括事件循环线程上的审计写入）会等。
单条语句微秒级；事务块的时长由调用方负责（账本一次记录是一小串读写 + 一次提交），
所以调用方只在必要处开事务，且不在事务里做慢操作。

写入一律同步 + 自动提交（``isolation_level=None``），单条 INSERT 微秒级，不引入写
线程、不做批处理。
"""

from __future__ import annotations

import sqlite3
import threading
import time
from collections.abc import Iterable, Iterator, Sequence
from contextlib import contextmanager
from pathlib import Path
from typing import Any

from wing.common.logger import log
from wing.db.migrations import MIGRATIONS, SCHEMA_VERSION

#: 打开重试的退避（基数 / 上限）与整个打开的总时限。只在**锁类**错误上重试，
#: 预算按时间算——到点即降级（它跑在事件循环线程上）。
_OPEN_RETRY_DELAY_S = 0.05
_OPEN_RETRY_DELAY_MAX_S = 0.2
_OPEN_DEADLINE_S = 2.0

#: 打开期（转 WAL + 迁移）的等锁上限；稳态值是 5000（见下）。
_OPEN_BUSY_TIMEOUT_MS = 200

#: 稳态写等锁上限（契约值：5000）。
_BUSY_TIMEOUT_MS = 5000


class SchemaVersionTooNew(RuntimeError):
    """磁盘上的 ``user_version`` 高于本代码认识的版本——不猜、不降级写。"""


class Transaction:
    """一次事务的句柄（:meth:`Database.transaction` 的产物）。

    ``live``：事务真的开着（``False`` = 库降级 / 开不了事务，块内不必再发语句）。
    ``committed``：块退出后为 ``True`` 才算落库——块内任一条语句失败、或 ``COMMIT``
    本身失败都会回滚。嵌套块拿到的是同一个句柄，提交结果因此只由最外层决定。
    """

    __slots__ = ("live", "committed")

    def __init__(self, *, live: bool) -> None:
        self.live = live
        self.committed = False


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
        # 跨线程共享单连接（见模块文档）：所有入口都在这把可重入锁内。可重入是必须的
        # ——`transaction()` 持锁跑完整个 with 块，块内的 execute / query 还要再进来。
        self._lock = threading.RLock()
        self._txn: Transaction | None = None
        self._txn_failed = False
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
        """建目录 → 连接 → pragma → 迁移。锁类错误重试到时限，其余立即降级。

        预算按**时间**算（``_OPEN_DEADLINE_S``），不按次数：迁移期的写锁冲突是立即
        失败（内层读快照让升级直接 BUSY，不走 busy handler），按次数算的预算会被
        这种「几乎不耗时」的尝试迅速烧穿。总时长上界必须存在——这个调用点在事件
        循环线程上（懒打开发生在 ``EventBus.emit`` 里）。
        """
        deadline = time.monotonic() + _OPEN_DEADLINE_S
        attempt = 0
        while True:
            attempt += 1
            try:
                self._open_once()
            except SchemaVersionTooNew as exc:
                self._degrade(exc)
                return
            except Exception as exc:
                self._release_connection()
                remaining = deadline - time.monotonic()
                if not _is_lock_error(exc) or remaining <= 0:
                    self._degrade(exc)
                    return
                backoff = min(_OPEN_RETRY_DELAY_S * attempt, _OPEN_RETRY_DELAY_MAX_S)
                time.sleep(min(backoff, remaining))
            else:
                # 成功路径放在 else 里：try 块之后不得有「顺路也能走到」的语句——
                # 失败分支睡完就该回到下一轮，绝不能落到成功。
                self._ready = True
                return

    def _open_once(self) -> None:
        self._path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(
            self._path, isolation_level=None, check_same_thread=False
        )
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
                # 版本判定必须在事务内重读：并发首开时另一个进程可能刚把这一版迁完
                # 并提交，此刻本次事务的快照仍是旧的——照旧跑 DDL 会撞
                # "table … already exists"（不是锁类错误，一次就把库判死）。重读即
                # 跳过；快照若真的过期，SQLite 拒绝在过期快照上写（database is
                # locked），落进外层重试。重读同样要复核「版本过新」：外层那次读
                # 也可能是过期快照（更新的代码刚迁完 v2），放过去就等于用旧 schema 写。
                observed = _user_version(conn)
                if observed > SCHEMA_VERSION:
                    raise SchemaVersionTooNew(
                        f"user_version={observed} > known {SCHEMA_VERSION}"
                    )
                if observed >= version:
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
        with self._lock:
            if not self._ready or self._conn is None:
                return False
            try:
                self._conn.execute(sql, params)
            except Exception as exc:
                self._fail(exc)
                return False
            return True

    def executemany(self, sql: str, rows: Iterable[Sequence[Any]]) -> bool:
        """执行一条写语句（多组参数）；语义同 :meth:`execute`。"""
        with self._lock:
            if not self._ready or self._conn is None:
                return False
            try:
                self._conn.executemany(sql, rows)
            except Exception as exc:
                self._fail(exc)
                return False
            return True

    def insert(self, sql: str, params: Sequence[Any] = ()) -> int | None:
        """执行一条 INSERT 并返回新行的 rowid；失败 / 降级返回 ``None``。

        「插入 + 取回 rowid」必须在同一把锁内完成：连接跨线程共享，两条语句之间
        挤进别的线程的 INSERT 就会把 id 抄成别人的（``last_insert_rowid`` 是连接级
        状态，不是语句级）。
        """
        with self._lock:
            if not self._ready or self._conn is None:
                return None
            try:
                cursor = self._conn.execute(sql, params)
            except Exception as exc:
                self._fail(exc)
                return None
            if cursor.lastrowid is None:
                return None
            return int(cursor.lastrowid)

    def query(self, sql: str, params: Sequence[Any] = ()) -> list[sqlite3.Row]:
        """执行查询。降级 / 出错一律返回空列表（不抛、不刷日志）。"""
        with self._lock:
            if not self._ready or self._conn is None:
                return []
            try:
                return self._conn.execute(sql, params).fetchall()
            except Exception:
                return []

    @contextmanager
    def transaction(self) -> Iterator[Transaction]:
        """把 with 块里的一切放进一个 ``BEGIN IMMEDIATE`` 事务（全有或全无）。

        yield 的是事务句柄：``live=False`` 表示库不可用或开不了事务（块内不必再发
        语句——``execute`` 会直接返回 ``False``、``query`` 是空列表）；块退出后读
        ``committed`` 才知道它**真的提交了**（块内任何一次写失败都会回滚，提交本身
        也可能失败）。只把语句的返回值当结论会漏掉这两种回滚。

        块内抛异常 → 回滚并把异常原样抛出（由调用方决定怎么兜）。

        事务期间持锁：同一进程内的其他线程会等到提交/回滚，这正是「读—改—写」需要
        的临界区（跨进程则由 SQLite 的写锁负责）。嵌套调用**并入外层事务**（拿到的
        是同一个句柄）——SQLite 没有嵌套事务，而分层（编排 → 存储）会让两端自然地
        各要一个原子块。
        """
        with self._lock:
            conn = self._conn
            if not self._ready or conn is None:
                yield Transaction(live=False)
                return
            open_txn = self._txn
            if open_txn is not None:
                yield open_txn  # 并入外层事务（SQLite 没有嵌套 BEGIN）
                return
            try:
                conn.execute("BEGIN IMMEDIATE")
            except Exception as exc:
                self._warn_write_error(exc)
                yield Transaction(live=False)
                return
            handle = Transaction(live=True)
            self._txn = handle
            self._txn_failed = False
            try:
                yield handle
            except BaseException:
                self._txn = None
                self._rollback()
                raise
            else:
                self._txn = None
                if self._txn_failed:
                    self._rollback()
                    return
                try:
                    conn.execute("COMMIT")
                except Exception as exc:
                    self._rollback()
                    self._warn_write_error(exc)
                    return
                handle.committed = True

    def close(self) -> None:
        """关闭连接（幂等；降级过的对象是 no-op）。"""
        with self._lock:
            self._ready = False
            conn, self._conn = self._conn, None
            if conn is None:
                return
            try:
                conn.close()
            except Exception:
                pass

    # ── 内部 ─────────────────────────────────────

    def _fail(self, exc: BaseException) -> None:
        """一条语句失败：标记事务不可提交 + 首次留一条 WARNING。"""
        if self._txn is not None:
            self._txn_failed = True
        self._warn_write_error(exc)

    def _rollback(self) -> None:
        self._txn_failed = True
        conn = self._conn
        if conn is None:
            return
        try:
            if conn.in_transaction:
                conn.execute("ROLLBACK")
        except Exception:
            pass

    def _warn_write_error(self, exc: BaseException) -> None:
        """首次写失败留一条 WARNING（正在丢数据），此后闭嘴——审计是 best-effort。"""
        if self._warned_write_error:
            return
        self._warned_write_error = True
        log.warning(
            f"wing.db write failed at {self._path}（后续写入静默丢弃）: {exc!r}"
        )
