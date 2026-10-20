# wing/context/cache_store.py
"""wing/context/cache_store.py — 缓存账本的唯一 SQL 处（`cache_call` / `cache_segment`）。

一个 :class:`CacheStore` 就是一个会话的账本仓储。`session_id` 是**存储里的真名**（不是
请求里的别名，见 `SessionManager.resolve_session_id`）——账本是按会话键存的行，写与读
必须落在同一个键上。

四件事决定了这张表长成现在这样：

- **段落表按指纹去重**：`fp` 是整份段列表的内容标识，同一份段列表在会话内只存一遍。
  一次 40 轮的会话因此是 40 行调用 + 每份段列表一份段落行，而不是 40 份副本。
- **参照调用靠指纹找**：候选的段列表是本次的前缀 ⟺ 它的 `fp` 等于「本次前 n 段」的
  指纹（n = 它的段数，`segment_count` 列）。于是判前缀不需要读任何段落表，也不必把每条
  历史调用的段落行逐段比对；「前 n 段的指纹」由调用方按候选惰性计算。
- **投影只取分叉位点那一段**（:meth:`CacheStore.segment_at`）：端点要的 `sha_before` /
  `sha_after` / 来源文件只有第 `fork.index` 段，读整份段列表纯属浪费。
- **会话维度归属**：裁剪、孤儿回收、会话间隔离都缩成「一条带 session_id 的 DELETE /
  SELECT」——不需要跨会话引用计数。

本层不判断 `Database.ready`：库不可用时 `query` 是空列表、写返回 ``False``，降级语义只在
`wing.db` 一处（与 `audit/recorder.py`、`wake/store.py` 同口径）。
"""

from __future__ import annotations

import sqlite3
from collections.abc import Sequence
from contextlib import AbstractContextManager

from wing.db import Database, get_database
from wing.schema import CacheCall, CacheFork, CachePart, CacheSource

_COLUMNS = (
    "id, ts, ts_ms, fp, purpose, model, provider, prompt, cached, "
    "expected, ref_fp, verdict, segment_count, "
    "fork_kind, fork_index, fork_label, fork_segments_affected, fork_tokens_lost"
)

#: 本会话账本的调用行（新→旧）。记账与投影共用：参照候选、分叉对照侧、端点投影
#: 都是「最近这些行」的同一份数据。
_RECENT = (
    f"SELECT {_COLUMNS} FROM cache_call WHERE session_id = ? ORDER BY id DESC LIMIT ?"
)

_SEGMENT_COLUMNS = "label, sha, chars, src_path, src_mtime, src_size"

_SEGMENT = (
    f"SELECT {_SEGMENT_COLUMNS} FROM cache_segment "
    "WHERE session_id = ? AND fp = ? AND idx = ?"
)

_SEGMENTS = (
    f"SELECT {_SEGMENT_COLUMNS} FROM cache_segment "
    "WHERE session_id = ? AND fp = ? ORDER BY idx"
)

#: 段列表的字符总量与「分叉位点之前」的字符量（`tokens_lost` 的估算依据）。
_CHAR_SUMS = (
    "SELECT COALESCE(SUM(chars), 0) AS total, "
    "COALESCE(SUM(CASE WHEN idx < ? THEN chars ELSE 0 END), 0) AS before "
    "FROM cache_segment WHERE session_id = ? AND fp = ?"
)

_INSERT_CALL = """
INSERT INTO cache_call (
    session_id, ts, ts_ms, fp, purpose, model, provider, prompt, cached,
    expected, ref_fp, verdict, segment_count,
    fork_kind, fork_index, fork_label, fork_segments_affected, fork_tokens_lost
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

_INSERT_SEGMENT = """
INSERT INTO cache_segment (
    session_id, fp, idx, label, sha, chars, src_path, src_mtime, src_size
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

_HAS_SEGMENTS = "SELECT 1 FROM cache_segment WHERE session_id = ? AND fp = ? LIMIT 1"

#: 环形裁剪的受害者：比「本会话第 keep 新的那条调用」更旧的全部调用。
#: 子查询返回 NULL（本会话不足 keep 条）时整体比较为 NULL，一条都不删。
_RING_PREDICATE = (
    "session_id = ?1 AND id <= ("
    "SELECT id FROM cache_call WHERE session_id = ?1 "
    "ORDER BY id DESC LIMIT 1 OFFSET ?2)"
)

_RING_VICTIMS = f"SELECT DISTINCT fp FROM cache_call WHERE {_RING_PREDICATE}"
_DELETE_RING = f"DELETE FROM cache_call WHERE {_RING_PREDICATE}"

#: 段落行随指纹最后一处引用一起回收（同会话内可能有多条调用共享同一份段列表）。
_DELETE_ORPHAN_SEGMENTS = (
    "DELETE FROM cache_segment WHERE session_id = ?1 AND fp = ?2 "
    "AND NOT EXISTS (SELECT 1 FROM cache_call WHERE session_id = ?1 AND fp = ?2)"
)


def _part(row: sqlite3.Row) -> CachePart:
    path = row["src_path"]
    source = (
        CacheSource(path=path, mtime=row["src_mtime"], size=row["src_size"])
        if path is not None
        else None
    )
    return CachePart(label=row["label"], sha=row["sha"], chars=row["chars"], src=source)


def _fork(row: sqlite3.Row) -> CacheFork | None:
    kind = row["fork_kind"]
    if kind is None:
        return None
    return CacheFork(
        kind=kind,
        index=row["fork_index"],
        label=row["fork_label"],
        segments_affected=row["fork_segments_affected"],
        tokens_lost=row["fork_tokens_lost"],
    )


def _call(row: sqlite3.Row) -> CacheCall:
    return CacheCall(
        ts=row["ts"],
        fp=row["fp"],
        purpose=row["purpose"],
        model=row["model"],
        provider=row["provider"],
        prompt=row["prompt"],
        cached=row["cached"],
        verdict=row["verdict"],
        segment_count=row["segment_count"],
        ts_ms=row["ts_ms"],
        expected=row["expected"],
        ref_fp=row["ref_fp"],
        fork=_fork(row),
    )


class CacheStore:
    """`cache_call` / `cache_segment` 的读写（会话维度，无缓存：每次现读库）。"""

    def __init__(self, session_id: str, database: Database | None = None) -> None:
        self._session_id = session_id
        self._database = database

    def database(self) -> Database:
        """进程级库单例（`database` 参数只给测试 / 显式装配用）。"""
        return self._database if self._database is not None else get_database()

    def transaction(self) -> AbstractContextManager[bool]:
        """把一次「读—改—写」放进同一个 `BEGIN IMMEDIATE`（见 `Database.transaction`）。"""
        return self.database().transaction()

    # ── 读 ───────────────────────────────────────

    def recent(self, limit: int) -> list[CacheCall]:
        """最近 `limit` 条调用，**新→旧**（账本的完整事实面）。

        记账要的就是这一份：最近一条（分叉的对照侧）+ 参照候选（`prompt > 0` 的行，
        判「段列表是不是本次的前缀」由调用方做）。会话的行数上界是 `MAX_CALLS`，
        所以「整份读」在这个设计里最多几百行小表数据。
        """
        rows = self.database().query(_RECENT, (self._session_id, limit))
        return [_call(row) for row in rows]

    def segment_at(self, fp: str | None, index: int) -> CachePart | None:
        """某个指纹的第 `index` 段（越界 / 指纹不存在 → None）。

        投影只需要分叉位点这一段：`sha_before` / `sha_after` / 来源文件都是它。
        """
        if not fp:
            return None
        rows = self.database().query(_SEGMENT, (self._session_id, fp, index))
        return _part(rows[0]) if rows else None

    def segments(self, fp: str) -> list[CachePart]:
        """整份段列表（按段序）。只在算「与上一次调用的最长公共前缀」时才需要。"""
        rows = self.database().query(_SEGMENTS, (self._session_id, fp))
        return [_part(row) for row in rows]

    def char_sums(self, fp: str, index: int) -> tuple[int, int]:
        """（段列表总字符数，前 `index` 段的字符数）——`tokens_lost` 的估算依据。"""
        rows = self.database().query(_CHAR_SUMS, (index, self._session_id, fp))
        if not rows:
            return 0, 0
        return int(rows[0]["total"]), int(rows[0]["before"])

    # ── 写 ───────────────────────────────────────

    def insert_call(self, call: CacheCall) -> bool:
        """追加一行调用（含分叉事实）。返回是否真的落库。"""
        fork = call.fork
        return self.database().execute(
            _INSERT_CALL,
            (
                self._session_id,
                call.ts,
                call.ts_ms,
                call.fp,
                call.purpose,
                call.model,
                call.provider,
                call.prompt,
                call.cached,
                call.expected,
                call.ref_fp,
                call.verdict,
                call.segment_count,
                fork.kind if fork is not None else None,
                fork.index if fork is not None else None,
                fork.label if fork is not None else None,
                fork.segments_affected if fork is not None else None,
                fork.tokens_lost if fork is not None else None,
            ),
        )

    def save_segments(self, fp: str, segments: Sequence[CachePart]) -> bool:
        """存一份段列表（同一指纹已经存过就跳过——内容寻址，同指纹即同内容）。"""
        if self.database().query(_HAS_SEGMENTS, (self._session_id, fp)):
            return True
        return self.database().executemany(
            _INSERT_SEGMENT,
            (
                (
                    self._session_id,
                    fp,
                    index,
                    segment.label,
                    segment.sha,
                    segment.chars,
                    segment.src.path if segment.src is not None else None,
                    segment.src.mtime if segment.src is not None else None,
                    segment.src.size if segment.src is not None else None,
                )
                for index, segment in enumerate(segments)
            ),
        )

    def trim(self, keep: int) -> bool:
        """环形上界：只留本会话最新的 `keep` 条调用，并回收失去引用的段落行。

        批量淘汰一次做完（一条 DELETE），不逐条：淘汰与回收都在**同一个事务**里，
        中途崩掉不会留下「调用没了、段落还在」的状态（反过来：段落比调用多活一会儿
        也无害——下次裁剪会再收一次）。

        Returns:
            写是否真的落库（`Database.execute` 的返回值）。没有需要淘汰的调用时也返回
            ``True``：那不是失败。
        """
        victims = [
            row["fp"]
            for row in self.database().query(_RING_VICTIMS, (self._session_id, keep))
        ]
        if not victims:
            return True
        if not self.database().execute(_DELETE_RING, (self._session_id, keep)):
            return False
        for fp in victims:
            # `?1` / `?2` 复用：显式编号的占位符只按最大序号取参数
            self.database().execute(_DELETE_ORPHAN_SEGMENTS, (self._session_id, fp))
        return True
