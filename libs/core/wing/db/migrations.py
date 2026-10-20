"""wing/db/migrations.py — 进程级 SQLite 的迁移脚本。

形状是**可追加的 tuple**：新版本一律「在末尾加一项」`(version, sql)`，版本号严格递增。
不要改成 dict / list 或其它形状——`Database` 只认这个契约。

`sql` 是一段**可以含多条语句**的脚本，由 :func:`wing.db.database.split_statements` 切开后
在同一事务里逐条执行，事务尾写回 `PRAGMA user_version`。因此单条迁移要么整体生效，
要么整体回滚——半截 schema 会让下一次启动重跑同一迁移并再次失败（永久降级）。

**迁移要保持短**（毫秒级，当前 v1 实测 p50 0.5ms）：库可能被多个进程同时首开，输家只有
「整个打开」的时间预算（`_OPEN_DEADLINE_S`，2 秒）等赢家迁完；迁移耗时逼近这个预算就会把
输家逼进降级（实测 1.5s 的迁移仍 0 降级，2.5s 开始出现降级；丢的只是那个进程的审计行）。
大搬迁请拆成多条小迁移，或挪到运行期后台任务里。
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
    (
        2,
        """
        -- 唤醒触发器（03_wake）：一条行 = 一个「什么事实/时刻该叫醒谁」的登记。
        -- kind='session' 由事件驱动（source_session_id 是订阅对象），kind='timer' 由
        -- next_fire_at_ms 驱动；remaining_done 对前者是 done 配额、对后者是剩余触发数。
        CREATE TABLE wake_trigger (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL CHECK (kind IN ('session','timer')),
            target_session_id TEXT NOT NULL,
            source_session_id TEXT,
            on_events TEXT NOT NULL DEFAULT 'done,ask',
            spec TEXT,
            next_fire_at_ms INTEGER,
            remaining_done INTEGER,
            prompt TEXT,
            state TEXT NOT NULL DEFAULT 'active',
            fire_count INTEGER NOT NULL DEFAULT 0,
            last_fire_at_ms INTEGER,
            last_error TEXT,
            created_at_ms INTEGER NOT NULL,
            created_by TEXT
        );
        CREATE INDEX idx_wake_source ON wake_trigger(source_session_id, state);
        CREATE INDEX idx_wake_next ON wake_trigger(state, next_fire_at_ms);
        """,
    ),
    (
        3,
        """
        -- 缓存账本（KV 前缀缓存）：一次 LLM 调用一行。与会话目录里的
        -- cache_ledger.json（旧版本写过）无关——旧文件不再读，也不再写。
        --
        -- 与 audit_* 的区别：审计是「发生过什么」的流水（只追加、永不裁）；这里是
        -- 「最近 MAX_CALLS 次调用」的**滚动窗口**——按会话裁剪，段落行随指纹回收。
        --
        -- 分叉事实（fork_*）与调用同生共死：要么四列非空，要么全空。
        -- fork_index = 与上一次调用的最长公共前缀长度；fork_tokens_lost 是估算值，
        -- 无参照时可为 NULL。
        CREATE TABLE cache_call (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            ts TEXT NOT NULL,
            ts_ms INTEGER,
            fp TEXT NOT NULL,
            purpose TEXT NOT NULL,
            model TEXT NOT NULL,
            provider TEXT NOT NULL,
            prompt INTEGER NOT NULL,
            cached INTEGER NOT NULL,
            expected INTEGER,
            ref_fp TEXT,
            verdict TEXT NOT NULL,
            -- 该调用前缀的段数（= 它那份段列表的长度）。前缀比较的边界：比本次
            -- 更长的候选连指纹都不必算。
            segment_count INTEGER NOT NULL,
            fork_kind TEXT,
            fork_index INTEGER,
            fork_label TEXT,
            fork_segments_affected INTEGER,
            fork_tokens_lost INTEGER,
            CHECK (
                (fork_kind IS NULL AND fork_index IS NULL AND fork_label IS NULL
                 AND fork_segments_affected IS NULL)
                OR (fork_kind IS NOT NULL AND fork_index IS NOT NULL
                    AND fork_label IS NOT NULL AND fork_segments_affected IS NOT NULL)
            )
        );
        -- id 是 rowid：索引项的隐式尾键就是它——「会话内新→旧」与环形裁剪都走这条。
        CREATE INDEX idx_cache_call_session ON cache_call(session_id, id);

        -- 一次调用的前缀被切成有序段（label / sha / chars / src）。主键
        -- (session_id, fp, idx)：fp 是整份段列表的指纹（内容寻址，同一份段列表在
        -- 会话内只存一遍），会话维度让「裁剪 / 归属 / 回收」都缩成一条 DELETE 说得清。
        CREATE TABLE cache_segment (
            session_id TEXT NOT NULL,
            fp TEXT NOT NULL,
            idx INTEGER NOT NULL,
            label TEXT NOT NULL,
            sha TEXT NOT NULL,
            chars INTEGER NOT NULL,
            src_path TEXT,
            src_mtime TEXT,
            src_size INTEGER,
            PRIMARY KEY (session_id, fp, idx)
        ) WITHOUT ROWID;
        """,
    ),
)

#: 代码认识的最高 schema 版本（= 最后一个迁移的目标版本）。
SCHEMA_VERSION: int = MIGRATIONS[-1][0]
