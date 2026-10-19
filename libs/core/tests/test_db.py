"""tests/test_db.py — wing.db 门面：迁移 / 降级 / 单例。

覆盖：
  - 迁移幂等（连开两次 user_version 不变、不报错）；三张表与索引都在；
  - 迁移失败整体回滚（user_version / DDL 零残留），修好后能续上；
  - 未知高版本 → ready=False 且不写；
  - 打开失败（路径被占成目录）→ ready=False、execute 返回 False、异常不外泄、只留一条 ERROR；
  - 首开锁竞态（并发转 WAL）→ 重试，不因良性冲突永久降级；
  - 运行期写失败 → 首次一条 WARNING，此后静默；
  - executemany / query 的正常与异常路径；
  - 单例 get / close / reset 语义。
"""

from __future__ import annotations

import os
import sqlite3
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
        assert _tables(db) == {"audit_llm_call", "audit_tool_call", "audit_compaction"}
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
        monkeypatch.setattr(db_mod, "MIGRATIONS", MIGRATIONS + ((2, _BAD_V2),))
        monkeypatch.setattr(db_mod, "SCHEMA_VERSION", 2)

        broken = Database(path)
        assert not broken.ready
        conn = sqlite3.connect(path)
        try:
            assert conn.execute("PRAGMA user_version").fetchone()[0] == 1
            names = {row[0] for row in conn.execute("SELECT name FROM sqlite_master")}
        finally:
            conn.close()
        assert "t2" not in names  # 失败那条 DDL 没有留痕
        assert {"audit_llm_call", "audit_tool_call", "audit_compaction"} <= names

        # 修好后同一个库能继续迁移（说明上面那次不是「半个 v2」）
        monkeypatch.setattr(db_mod, "MIGRATIONS", MIGRATIONS + ((2, _GOOD_V2),))
        fixed = Database(path)
        assert fixed.ready
        assert fixed.query("PRAGMA user_version")[0][0] == 2
        assert "t2" in _tables(fixed)
        fixed.close()


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

        monkeypatch.setattr(db_mod, "_OPEN_ATTEMPTS", 3)
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

        monkeypatch.setattr(db_mod, "_OPEN_ATTEMPTS", 3)
        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0)
        attempts = _flaky_open(
            monkeypatch, fail_times=99, message="unable to open database file"
        )
        blocked = tmp_path / "wing.db"
        blocked.mkdir()

        db = Database(blocked)

        assert not db.ready
        assert len(attempts) == 1

    def test_exhausted_retries_degrade(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_ATTEMPTS", 3)
        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0)
        attempts = _flaky_open(monkeypatch, fail_times=99)

        db = Database(tmp_path / "wing.db")

        assert not db.ready
        assert len(attempts) == 3

    def test_real_lock_from_another_connection_is_retryable(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """真 SQLite 的锁冲突（另一连接持写事务）走的就是重试路径，不是「库坏了」。"""
        import wing.db.database as db_mod

        monkeypatch.setattr(db_mod, "_OPEN_BUSY_TIMEOUT_MS", 20)
        monkeypatch.setattr(db_mod, "_OPEN_ATTEMPTS", 2)
        monkeypatch.setattr(db_mod, "_OPEN_RETRY_DELAY_S", 0)

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

        # 锁放开后同一个库重新打开即成功——上面那次降级没有污染文件
        assert Database(path).ready


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

    def test_query_error_returns_empty(self, tmp_path: Path) -> None:
        """查询出错返回空列表（不抛、不刷日志）。"""
        db = Database(tmp_path / "wing.db")

        assert db.query("SELECT * FROM no_such_table") == []

        db.close()


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
