"""wing/db/migrations.py — 进程级 SQLite 的迁移脚本。

形状是**可追加的 tuple**：新版本一律「在末尾加一项」`(version, sql)`，版本号严格递增。
不要改成 dict / list 或其它形状——`Database` 只认这个契约。

`sql` 是一段**可以含多条语句**的脚本，由 :func:`wing.db.database.split_statements` 切开后
在同一事务里逐条执行，事务尾写回 `PRAGMA user_version`。因此单条迁移要么整体生效，
要么整体回滚——半截 schema 会让下一次启动重跑同一迁移并再次失败（永久降级）。

**迁移要保持短**（毫秒级，当前 v1 实测 p50 0.5ms）：库可能被多个进程同时首开，输家只有
「整个打开」的时间预算（`_OPEN_DEADLINE_S`，2 秒）等赢家迁完；秒级的迁移会把输家逼进
降级（丢的只是那个进程的审计行）。大搬迁请拆成多条小迁移，或挪到运行期后台任务里。
"""

from __future__ import annotations

#: 迁移列表：(目标 user_version, SQL 脚本)。版本号从 1 起、严格递增。
MIGRATIONS: tuple[tuple[int, str], ...] = (
    (
        1,
        """
        -- 审计：一次 LLM 调用一行（append-only）。
        CREATE TABLE audit_llm_call (
            id INTEGER PRIMARY KEY,
            ts_ms INTEGER NOT NULL,
            ts TEXT NOT NULL,
            session_id TEXT,
            request_id TEXT,
            model TEXT NOT NULL,
            provider TEXT,
            prompt_tokens INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            cached_tokens INTEGER NOT NULL DEFAULT 0,
            first_chunk_rt_ms REAL,
            tokens_per_sec REAL,
            duration_ms INTEGER,
            stop_reason TEXT
        );
        CREATE INDEX idx_audit_llm_call_ts ON audit_llm_call(ts_ms);
        CREATE INDEX idx_audit_llm_call_session ON audit_llm_call(session_id, ts_ms);

        -- 审计：一次**真实执行**的工具调用一行（中断合成结果不写，见 audit/recorder.py）。
        CREATE TABLE audit_tool_call (
            id INTEGER PRIMARY KEY,
            ts_ms INTEGER NOT NULL,
            ts TEXT NOT NULL,
            session_id TEXT,
            request_id TEXT,
            tool_name TEXT NOT NULL,
            tool_call_id TEXT,
            success INTEGER NOT NULL,
            duration_ms INTEGER
        );
        CREATE INDEX idx_audit_tool_call_ts ON audit_tool_call(ts_ms);
        CREATE INDEX idx_audit_tool_call_session ON audit_tool_call(session_id, ts_ms);

        -- 审计：一次上下文压缩一行。
        CREATE TABLE audit_compaction (
            id INTEGER PRIMARY KEY,
            ts_ms INTEGER NOT NULL,
            ts TEXT NOT NULL,
            session_id TEXT NOT NULL,
            model TEXT,
            original_tokens INTEGER,
            compressed_tokens INTEGER
        );
        CREATE INDEX idx_audit_compaction_session ON audit_compaction(session_id, ts_ms);
        """,
    ),
)

#: 代码认识的最高 schema 版本（= 最后一个迁移的目标版本）。
SCHEMA_VERSION: int = MIGRATIONS[-1][0]
