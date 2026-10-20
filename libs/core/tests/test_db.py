"""tests/test_db.py — wing.db 门面：迁移 / 降级 / 单例。

覆盖：
  - 迁移幂等（连开两次 user_version 不变、不报错）；三张表与索引都在；
  - 迁移失败整体回滚（user_version / DDL 零残留），修好后能续上；
  - 未知高版本 → ready=False 且不写；
  - 打开失败（路径被占成目录）→ ready=False、execute 返回 False、异常不外泄、只留一条 ERROR；
  - 首开锁竞态（并发转 WAL）→ 重试，不因良性冲突永久降级；
  - 迁移期并发（过期快照读到旧版本）→ 拿到写锁后重读版本并跳过，不撞「表已存在」；
  - 运行期写失败 → 首次一条 WARNING，此后静默；
  - executemany / query 的正常与异常路径；
  - 单例 get / close / reset 语义。
"""

from __future__ import annotations

import os
import sqlite3
import threading
import time
from pathlib import Path
from typing import Any, cast

import pytest

from wing.db import close_database, get_database, reset_database
from wing.db.database import Database, split_statements
from wing.db.migrations import MIGRATIONS, SCHEMA_VERSION


#: 迁移回滚测试的两条 v2：坏的（第二句引用不存在的表）与好的。
_BAD_V2 = """
CREATE TABLE t2 (x INTEGER);
INSERT INTO no_such_table (x) VALUES (1);
"""
_GOOD_V2 = "CREATE TABLE t2 (x INTEGER);"


def _tables(db: Database) -> set[str]:
    rows = db.query("SELECT name FROM sqlite_master WHERE type='table'")
    return {row["name"] for row in rows}


def _indexes(db: Database) -> set[str]:
    rows = db.query("SELECT name FROM sqlite_master WHERE type='index'")
    return {row["name"] for row in rows}


# ============================================================
# 迁移
# ============================================================


class TestMigrations:
    def test_fresh_open_creates_schema_and_sets_version(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")
        assert db.ready
        assert _tables(db) == {
            "audit_llm_call",
            "audit_tool_call",
            "audit_compaction",
            "wake_trigger",
        }
        assert {
            "idx_audit_llm_call_ts",
            "idx_audit_llm_call_session",
            "idx_audit_tool_call_ts",
            "idx_audit_tool_call_session",
            "idx_audit_compaction_session",
        } <= _indexes(db)
        assert db.query("PRAGMA user_version")[0][0] == SCHEMA_VERSION
        db.close()

    def test_reopen_is_idempotent(self, tmp_path: Path) -> None:
        """连开两次：版本不变、不报错、数据还在（迁移按版本跳过）。"""
        path = tmp_path / "wing.db"
        first = Database(path)
        assert first.execute(
            "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (?, ?, ?)",
            (1, "2024-01-01T00:00:00", "m"),
        )
        first.close()

        second = Database(path)
        assert second.ready
        assert second.query("PRAGMA user_version")[0][0] == SCHEMA_VERSION
        assert len(second.query("SELECT * FROM audit_llm_call")) == 1
        second.close()

    def test_v1_database_upgrades_in_place(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """已发布的 v1 库（只有三张审计表 + 已写入的行）在**原位**升到当前版本。

        这是跨分支唯一的数据路径：#242 已把 v1 发出去，#243 在末尾追加了 v2
        （``wake_trigger``）。升级必须满足三件事——版本号推进、新表建成、旧行原样还在
        （追加迁移不许重写既有数据，也不许让老库重新走一遍 v1）。
        """
        import wing.db.database as db_mod

        path = tmp_path / "wing.db"

        # ① 用「只到 v1」的迁移表建库（模拟 #242 时期的进程），并写两行审计事实。
        monkeypatch.setattr(db_mod, "MIGRATIONS", MIGRATIONS[:1])
        monkeypatch.setattr(db_mod, "SCHEMA_VERSION", MIGRATIONS[0][0])
        v1 = Database(path)
        assert v1.ready
        assert v1.query("PRAGMA user_version")[0][0] == 1
        assert "wake_trigger" not in _tables(v1)
        for row in ((1, "2024-01-01T00:00:00", "m1"), (2, "2024-01-01T00:00:01", "m2")):
            assert v1.execute(
                "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (?, ?, ?)", row
            )
        v1.close()

        # ② 用当前代码（完整 MIGRATIONS）打开同一个库：v2 追加、v1 的数据不动。
        monkeypatch.undo()
        upgraded = Database(path)
        assert upgraded.ready
        assert upgraded.query("PRAGMA user_version")[0][0] == SCHEMA_VERSION
        assert "wake_trigger" in _tables(upgraded)
        rows = upgraded.query(
            "SELECT ts_ms, ts, model FROM audit_llm_call ORDER BY ts_ms"
        )
        assert [tuple(row) for row in rows] == [
            (1, "2024-01-01T00:00:00", "m1"),
            (2, "2024-01-01T00:00:01", "m2"),
        ]
        # 升级后的库能写新表（唤醒行），也能继续写老表
        assert upgraded.execute(
            "INSERT INTO wake_trigger (kind, target_session_id, on_events,"
            " state, created_at_ms) VALUES ('session', 'p', 'done,ask', 'active', 1)"
        )
        assert len(upgraded.query("SELECT * FROM wake_trigger")) == 1
        upgraded.close()

    def test_migration_list_is_appendable_shape(self) -> None:
        """契约：MIGRATIONS 是递增的 (version, sql) tuple（03_wake 会在末尾追加）。"""
        versions = [version for version, _ in MIGRATIONS]
        assert isinstance(MIGRATIONS, tuple)
        assert versions == sorted(versions)
        assert len(set(versions)) == len(versions)
        assert versions and versions[0] == 1
        assert SCHEMA_VERSION == versions[-1]

    def test_pragmas_applied(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")
        assert db.query("PRAGMA journal_mode")[0][0].lower() == "wal"
        # synchronous=NORMAL 为 1
        assert db.query("PRAGMA synchronous")[0][0] == 1
        assert db.query("PRAGMA busy_timeout")[0][0] == 5000
        db.close()

    def test_split_statements_keeps_comments_with_following_statement(self) -> None:
        statements = split_statements("-- a comment\nCREATE TABLE t (x);\n\nSELECT 1;")
        assert len(statements) == 2
        assert "CREATE TABLE" in statements[0]
        assert statements[1].strip() == "SELECT 1;"


class TestMigrationRollback:
    def test_failed_migration_rolls_back_then_retries_next_open(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """坏迁移整体回滚（半截 schema 会让下次启动永久失败），修好后能续上。"""
        import wing.db.database as db_mod

        path = tmp_path / "wing.db"
        next_version = SCHEMA_VERSION + 1
        monkeypatch.setattr(
            db_mod, "MIGRATIONS", MIGRATIONS + ((next_version, _BAD_V2),)
        )
        monkeypatch.setattr(db_mod, "SCHEMA_VERSION", next_version)

        broken = Database(path)
        assert not broken.ready
        conn = sqlite3.connect(path)
        try:
            assert conn.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
            names = {row[0] for row in conn.execute("SELECT name FROM sqlite_master")}
        finally:
            conn.close()
        assert "t2" not in names  # 失败那条 DDL 没有留痕
        assert {"audit_llm_call", "audit_tool_call", "audit_compaction"} <= names

        # 修好后同一个库能继续迁移（说明上面那次不是「半个 v2」）
        monkeypatch.setattr(
            db_mod, "MIGRATIONS", MIGRATIONS + ((next_version, _GOOD_V2),)
        )
        fixed = Database(path)
        assert fixed.ready
        assert fixed.query("PRAGMA user_version")[0][0] == next_version
        assert "t2" in _tables(fixed)
        fixed.close()


class TestStaleMigrationSnapshot:
    def test_already_applied_version_is_skipped_inside_the_transaction(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """版本判定必须在**拿到写锁之后**重读——否则并发首开会把库判死。

        并发首开的时序：赢家进了迁移事务（未提交），输家读到旧版本（0）→ 也进来跑
        DDL → 拿到锁时赢家已提交 → "table … already exists"（**不是**锁类错误，
        一次就永久降级）。这里用「同一版本出现两次」把那个**过期快照**确定性地做出来
        （循环外读到的 current 是 0，第一条迁完后它不是 1 了，但局部变量没更新）。
        """
        import wing.db.database as db_mod

        v1_script = MIGRATIONS[0][1]
        monkeypatch.setattr(db_mod, "MIGRATIONS", ((1, v1_script), (1, v1_script)))
        monkeypatch.setattr(db_mod, "SCHEMA_VERSION", 1)

        db = Database(tmp_path / "wing.db")

        assert db.ready
        assert _tables(db) == {"audit_llm_call", "audit_tool_call", "audit_compaction"}
        assert db.query("PRAGMA user_version")[0][0] == 1
        db.close()


def test_too_new_version_is_caught_inside_the_transaction(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """外层那次读也可能是过期快照：事务内重读到的「版本过新」必须同样降级。

    竞态形状：更新的代码刚把库迁到 v2，本进程外层的读仍看到 0 —— 若只在事务外判
    「版本过新」，这里会一路 `COMMIT` + 跳过，接着用旧 schema 写库。
    """
    import wing.db.database as db_mod

    path = tmp_path / "wing.db"
    too_new = SCHEMA_VERSION + 1
    conn = sqlite3.connect(path, isolation_level=None)
    conn.execute("CREATE TABLE marker (x INTEGER)")
    conn.execute(f"PRAGMA user_version = {too_new}")
    conn.close()

    real = db_mod._user_version
    reads: list[int] = []

    def stale_first_read(conn: sqlite3.Connection) -> int:
        reads.append(1)
        return 0 if len(reads) == 1 else real(conn)

    monkeypatch.setattr(db_mod, "_user_version", stale_first_read)

    db = Database(path)

    assert not db.ready
    assert len(reads) == 2  # 外层一次 + 事务内一次
    conn = sqlite3.connect(path)
    try:
        # 不猜、不降级写
        assert conn.execute("PRAGMA user_version").fetchone()[0] == too_new
    finally:
        conn.close()


# ============================================================
# 首开锁竞态
# ============================================================


class TestFirstOpenLockRetry:
    """并发首次打开会为「转 WAL」抢排他锁：输家拿到 locked，是良性竞态。

    降级是「库坏了」的兜底，不能被这种冲突触发（`_ready` 是粘性的，一旦降级该
    进程的审计行就全丢）。
    """

    def test_lock_error_is_retried_then_succeeds(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0)
        attempts = _flaky_open(monkeypatch, fail_times=1)

        db = Database(tmp_path / "wing.db")

        assert db.ready
        assert len(attempts) == 2  # 首次 locked → 重试成功
        db.close()

    def test_non_lock_error_is_not_retried(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """打不开文件（路径被占成目录）不是竞态，重试只是白等。"""
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0)
        attempts = _flaky_open(
            monkeypatch, fail_times=99, message="unable to open database file"
        )

        db = Database(tmp_path / "wing.db")

        assert not db.ready
        assert len(attempts) == 1

    def test_retries_until_deadline_then_degrades(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """锁一直在 → 在时限内反复重试，到点才降级（不是第一次失败就放弃）。"""
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0.01)
        monkeypatch.setattr(db_mod, "_OPEN_DEADLINE_S", 0.1)
        attempts = _flaky_open(monkeypatch, fail_times=10**9)

        db = Database(tmp_path / "wing.db")

        assert not db.ready
        assert len(attempts) > 1

    def test_loser_waits_for_a_slow_winner(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """赢家 0.6s 才迁完：输家靠**时间**预算等到它（按次数的预算会在 ~500ms 放弃）。"""
        attempts = _failing_open_until(monkeypatch, release_after_s=0.6)
        started = time.monotonic()

        db = Database(tmp_path / "wing.db")

        assert db.ready
        assert len(attempts) >= 5
        assert time.monotonic() - started >= 0.6
        db.close()

    def test_deadline_exhausted_degrades_immediately(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """时限已到 → 锁类错误也不再重试（总时长上界是硬约束）。"""
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0)
        monkeypatch.setattr(db_mod, "_OPEN_DEADLINE_S", 0)
        attempts = _flaky_open(monkeypatch, fail_times=1)

        db = Database(tmp_path / "wing.db")

        assert not db.ready
        assert len(attempts) == 1

    def test_real_lock_from_another_connection_is_retryable(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, wing_warnings: list[str]
    ) -> None:
        """真 SQLite 的锁冲突（另一连接持写事务）走的就是重试路径，不是「库坏了」。"""
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_BUSY_TIMEOUT_MS", 20)
        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0.01)
        monkeypatch.setattr(db_mod, "_OPEN_DEADLINE_S", 0.2)
        attempts = _counting_open(monkeypatch)

        path = tmp_path / "wing.db"
        holder = sqlite3.connect(path, isolation_level=None)
        try:
            holder.execute("CREATE TABLE holder (x INTEGER)")
            holder.execute("BEGIN")
            holder.execute("INSERT INTO holder (x) VALUES (1)")

            blocked = Database(path)
            assert not blocked.ready
        finally:
            holder.execute("ROLLBACK")
            holder.close()

        # 真的是锁类错误，且确实在时限内反复重试（不是别的错误恰好降级）
        assert len(attempts) >= 2
        assert any(
            "wing.db disabled" in message and "database is locked" in message
            for message in wing_warnings
        ), wing_warnings

        # 锁放开后同一个库重新打开即成功——上面那次降级没有污染文件
        assert Database(path).ready

    def test_real_non_lock_error_is_tried_once(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """路径被占成目录走的是真实错误路径（不是人造异常），同样只尝试一次。"""
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0)
        attempts = _counting_open(monkeypatch)
        blocked = tmp_path / "wing.db"
        blocked.mkdir()

        db = Database(blocked)

        assert not db.ready
        assert len(attempts) == 1


def _failing_open_until(
    monkeypatch: pytest.MonkeyPatch, *, release_after_s: float
) -> list[int]:
    """``_open_once`` 在 ``release_after_s`` 秒内一直抛锁错误，之后转发真实实现。

    模拟「赢家在慢慢迁移」：输家能不能撑过去，取决于预算怎么算（时间 vs 次数）。
    """
    import wing.db.database as db_mod

    real = db_mod.Database._open_once
    started = time.monotonic()
    attempts: list[int] = []

    def flaky(self: Database) -> None:
        attempts.append(len(attempts) + 1)
        if time.monotonic() - started < release_after_s:
            raise sqlite3.OperationalError("database is locked")
        real(self)

    monkeypatch.setattr(db_mod.Database, "_open_once", flaky)
    return attempts


def _counting_open(monkeypatch: pytest.MonkeyPatch) -> list[int]:
    """包一层 ``Database._open_once`` 记尝试次数（转发到真实实现，不注入失败）。"""
    import wing.db.database as db_mod

    real = db_mod.Database._open_once
    attempts: list[int] = []

    def wrapped(self: Database) -> None:
        attempts.append(len(attempts) + 1)
        real(self)

    monkeypatch.setattr(db_mod.Database, "_open_once", wrapped)
    return attempts


def _flaky_open(
    monkeypatch: pytest.MonkeyPatch,
    *,
    fail_times: int,
    message: str = "database is locked",
) -> list[int]:
    """让 ``Database._open_once`` 前 ``fail_times`` 次抛 ``OperationalError(message)``。

    返回「尝试次数」的活列表（长度即真实尝试次数）。
    """
    import wing.db.database as db_mod

    real = db_mod.Database._open_once
    attempts: list[int] = []

    def flaky(self: Database) -> None:
        attempts.append(len(attempts) + 1)
        if len(attempts) <= fail_times:
            raise sqlite3.OperationalError(message)
        real(self)

    monkeypatch.setattr(db_mod.Database, "_open_once", flaky)
    return attempts


# ============================================================
# 读 / 写
# ============================================================


class TestExecuteAndQuery:
    def test_executemany_writes_all_rows(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")

        assert db.executemany(
            "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
            [(1, "t1", "s1"), (2, "t2", "s2")],
        )
        assert [
            row["session_id"] for row in db.query("SELECT * FROM audit_compaction")
        ] == [
            "s1",
            "s2",
        ]
        db.close()

    def test_executemany_error_returns_false(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")

        assert (
            db.executemany("INSERT INTO no_such_table (x) VALUES (?)", [(1,), (2,)])
            is False
        )
        db.close()

    def test_query_error_returns_empty(self, tmp_path: Path) -> None:
        """查询出错返回空列表（不抛、不刷日志）。"""
        db = Database(tmp_path / "wing.db")

        assert db.query("SELECT * FROM no_such_table") == []

        db.close()


class TestInsert:
    def test_insert_returns_the_new_rowid(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")
        first = db.insert(
            "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (?, ?, ?)",
            (1, "t", "m"),
        )
        second = db.insert(
            "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (?, ?, ?)",
            (2, "t", "m"),
        )
        assert first == 1 and second == 2
        db.close()

    def test_insert_error_returns_none(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")
        assert db.insert("INSERT INTO no_such_table (x) VALUES (?)", (1,)) is None
        db.close()

    def test_insert_and_rowid_survive_another_thread(self, tmp_path: Path) -> None:
        """写入 + 取回 rowid 跨线程互不串台（`last_insert_rowid` 是连接级状态）。"""
        db = Database(tmp_path / "wing.db")
        results: list[int | None] = []
        barrier = threading.Barrier(4)

        def write() -> None:
            barrier.wait()
            for _ in range(20):
                results.append(
                    db.insert(
                        "INSERT INTO audit_llm_call (ts_ms, ts, model) "
                        "VALUES (?, ?, ?)",
                        (1, "t", "m"),
                    )
                )

        threads = [threading.Thread(target=write) for _ in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        # 每个线程拿到的 id 各不相同，恰好覆盖全表（没有一次抄到别人的 rowid）
        assert sorted(cast("list[int]", results)) == list(range(1, 81))
        db.close()


class TestTransactions:
    def test_commit_persists_every_statement(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")
        with db.transaction() as live:
            assert live is True
            assert db.execute(
                "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
                (1, "t", "s"),
            )
            assert db.execute(
                "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
                (2, "t", "s"),
            )
        assert len(db.query("SELECT * FROM audit_compaction")) == 2
        db.close()

    def test_a_failed_statement_rolls_the_whole_block_back(
        self, tmp_path: Path
    ) -> None:
        """块内一次写失败 ⇒ 整块回滚（不提交半截），且不回抛。"""
        db = Database(tmp_path / "wing.db")
        with db.transaction():
            assert db.execute(
                "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
                (1, "t", "s"),
            )
            assert db.execute("INSERT INTO no_such_table (x) VALUES (?)", (1,)) is False
        assert db.query("SELECT * FROM audit_compaction") == []
        # 回滚之后仍可正常写（事务状态没有粘住）
        assert db.execute(
            "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
            (3, "t", "s"),
        )
        assert len(db.query("SELECT * FROM audit_compaction")) == 1
        db.close()

    def test_an_exception_rolls_back_and_propagates(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")
        with pytest.raises(RuntimeError):
            with db.transaction():
                db.execute(
                    "INSERT INTO audit_compaction (ts_ms, ts, session_id) "
                    "VALUES (?, ?, ?)",
                    (1, "t", "s"),
                )
                raise RuntimeError("boom")
        assert db.query("SELECT * FROM audit_compaction") == []
        db.close()

    def test_nested_blocks_join_the_outer_transaction(self, tmp_path: Path) -> None:
        """嵌套不另起 BEGIN（SQLite 不支持）：内层看到的是同一笔事务。"""
        db = Database(tmp_path / "wing.db")
        with db.transaction():
            db.execute(
                "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
                (1, "t", "s"),
            )
            with db.transaction() as inner:
                assert inner is True
                db.execute(
                    "INSERT INTO audit_compaction (ts_ms, ts, session_id) "
                    "VALUES (?, ?, ?)",
                    (2, "t", "s"),
                )
        assert len(db.query("SELECT * FROM audit_compaction")) == 2
        db.close()

    def test_degraded_database_yields_false(self, tmp_path: Path) -> None:
        """库打不开时事务块 yield False（调用方据此跳过整段工作）。"""
        blocker = tmp_path / "wing.db"
        blocker.mkdir()
        db = Database(blocker)
        assert not db.ready
        with db.transaction() as live:
            assert live is False
        db.close()

    def test_reads_inside_the_block_see_uncommitted_writes(
        self, tmp_path: Path
    ) -> None:
        """读—改—写要在同一笔事务里：块内的 SELECT 看得到块内刚写的行。"""
        db = Database(tmp_path / "wing.db")
        with db.transaction():
            db.execute(
                "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
                (1, "t", "s"),
            )
            assert len(db.query("SELECT * FROM audit_compaction")) == 1
        db.close()


class TestCrossThreadUse:
    def test_queries_from_a_worker_thread(self, tmp_path: Path) -> None:
        """连接跨线程共享（`check_same_thread=False`）：工作线程能读也能写。"""
        db = Database(tmp_path / "wing.db")
        assert db.execute(
            "INSERT INTO audit_compaction (ts_ms, ts, session_id) VALUES (?, ?, ?)",
            (1, "t", "main"),
        )
        seen: list[Any] = []

        def worker() -> None:
            seen.append(len(db.query("SELECT * FROM audit_compaction")))
            seen.append(
                db.execute(
                    "INSERT INTO audit_compaction (ts_ms, ts, session_id) "
                    "VALUES (?, ?, ?)",
                    (2, "t", "worker"),
                )
            )

        thread = threading.Thread(target=worker)
        thread.start()
        thread.join()
        assert seen == [1, True]
        assert len(db.query("SELECT * FROM audit_compaction")) == 2
        db.close()

    def test_concurrent_transactions_do_not_interleave(self, tmp_path: Path) -> None:
        """两个线程各自的「读—改—写」不能交叉：簿记结果必须精确。"""
        db = Database(tmp_path / "wing.db")
        db.execute(
            "INSERT INTO audit_compaction (ts_ms, ts, session_id, original_tokens) "
            "VALUES (?, ?, ?, ?)",
            (1, "t", "s", 0),
        )
        barrier = threading.Barrier(8)

        def bump() -> None:
            barrier.wait()
            for _ in range(10):
                with db.transaction():
                    row = db.query("SELECT original_tokens AS n FROM audit_compaction")[
                        0
                    ]
                    db.execute(
                        "UPDATE audit_compaction SET original_tokens = ?",
                        (int(row["n"]) + 1,),
                    )

        threads = [threading.Thread(target=bump) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        assert (
            db.query("SELECT original_tokens AS n FROM audit_compaction")[0]["n"] == 80
        )
        db.close()

    def test_singleton_is_created_once_under_contention(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """首次访问可能来自任一工作线程：并发 `get_database()` 只能开出一份连接。"""
        monkeypatch.setenv("WING_HOME", str(tmp_path / "home"))
        reset_database()
        created: list[Any] = []
        barrier = threading.Barrier(6)

        def grab() -> None:
            barrier.wait()
            created.append(get_database())

        threads = [threading.Thread(target=grab) for _ in range(6)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        assert len({id(database) for database in created}) == 1
        assert all(database.ready for database in created)
        reset_database()


class TestSchemaTooNew:
    def test_higher_version_degrades_without_writing(self, tmp_path: Path) -> None:
        path = tmp_path / "wing.db"
        conn = sqlite3.connect(path)
        conn.execute(f"PRAGMA user_version = {SCHEMA_VERSION + 5}")
        conn.commit()
        conn.close()

        db = Database(path)
        assert not db.ready
        assert (
            db.execute(
                "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (?, ?, ?)",
                (1, "t", "m"),
            )
            is False
        )
        assert db.query("SELECT 1") == []
        # 不迁移、不降级写：版本原样保留
        conn = sqlite3.connect(path)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION + 5
        conn.close()


# ============================================================
# 降级
# ============================================================


class TestDegradation:
    def test_unopenable_path_degrades_silently(self, tmp_path: Path) -> None:
        """路径被占成目录 → 打不开 → ready=False、写返回 False、无异常、只留一条 ERROR。"""
        blocked = tmp_path / "wing.db"
        blocked.mkdir()

        db = Database(blocked)
        assert not db.ready
        assert (
            db.execute(
                "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (1, 't', 'm')"
            )
            is False
        )
        assert (
            db.executemany(
                "INSERT INTO audit_tool_call (ts_ms, ts, tool_name, success) VALUES (?,?,?,?)",
                [(1, "t", "Bash", 1)],
            )
            is False
        )
        assert db.query("SELECT 1") == []
        # 降级后 close 幂等
        db.close()
        db.close()

    def test_error_logged_once_at_open(
        self, tmp_path: Path, wing_warnings: list[str]
    ) -> None:
        """打开失败留一条痕（ERROR 级别由 wing_warnings 的 handler 捕获），之后不再刷。"""
        blocked = tmp_path / "wing.db"
        blocked.mkdir()
        db = Database(blocked)
        for _ in range(5):
            db.execute("SELECT 1")
        assert len([m for m in wing_warnings if "wing.db disabled" in m]) == 1

    def test_write_failure_warns_once_then_silent(
        self, tmp_path: Path, wing_warnings: list[str]
    ) -> None:
        """打开成功但语句错 → 首次一条 WARNING，之后同样失败不再刷日志。"""
        db = Database(tmp_path / "wing.db")
        assert db.ready
        for _ in range(3):
            assert db.execute("INSERT INTO no_such_table (x) VALUES (1)") is False
        warnings = [m for m in wing_warnings if "wing.db write failed" in m]
        assert len(warnings) == 1
        # 连不上不是它的错：健康语句照常成功
        assert db.execute(
            "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (?, ?, ?)",
            (1, "t", "m"),
        )
        db.close()

    def test_bad_sql_does_not_escape(self, tmp_path: Path) -> None:
        """任何异常都不外泄——包括**不是** sqlite3.Error 的那一类。"""
        db = Database(tmp_path / "wing.db")
        # sql 不是 str → TypeError（非 sqlite3.Error，仍须吞掉）
        assert db.execute(cast(Any, None)) is False
        db.close()


# ============================================================
# 单例
# ============================================================


class TestSingleton:
    def test_get_close_reset(self) -> None:
        close_database()
        home = Path(os.environ["WING_HOME"])
        assert get_database().path == home / "core" / "wing.db"
        assert get_database() is get_database()

        first = get_database()
        close_database()
        assert get_database() is not first

        second = get_database()
        reset_database()
        assert get_database() is not second

    def test_close_is_idempotent(self) -> None:
        get_database()
        close_database()
        close_database()
