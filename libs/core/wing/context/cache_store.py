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
from typing import Any

from wing.db import Database, get_database
from wing.db.database import Transaction
from wing.schema import CacheCall, CacheFork, CachePart, CacheSource

#: 记账等写锁的上限：远短于 `Database` 的稳态值（5000）。账本是 best-effort 的诊断
#: 数据，跨进程写锁竞争时宁可丢这一笔，也不要把本进程的读路径拖成秒级。
_BUSY_TIMEOUT_MS = 250

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

_DELETE_CALLS = "DELETE FROM cache_call WHERE session_id = ?"
_DELETE_SEGMENTS = "DELETE FROM cache_segment WHERE session_id = ?"


def _part(row: sqlite3.Row) -> CachePart:
    path = row["src_path"]
    source = (
        CacheSource(path=path, mtime=row["src_mtime"], size=row["src_size"])
        if path is not None
        else None
    )
    return CachePart(label=row["label"], sha=row["sha"], chars=row["chars"], src=source)


def _fork(row: sqlite3.Row) -> CacheFork | None:
    kind, label = row["fork_kind"], row["fork_label"]
    index, affected = _int(row["fork_index"]), _int(row["fork_segments_affected"])
    if not isinstance(kind, str) or not isinstance(label, str):
        return None
    if index is None or affected is None:
        return None
    return CacheFork(
        kind=kind,
        index=index,
        label=label,
        segments_affected=affected,
        tokens_lost=_int(row["fork_tokens_lost"]),
    )


def _is_int(value: Any) -> bool:
    """真整数才算计数：`bool` 是 `int` 的子类，不该被当成 1。"""
    return isinstance(value, int) and not isinstance(value, bool)


def _int(value: Any) -> int | None:
    return value if _is_int(value) else None


def _call(row: sqlite3.Row) -> CacheCall | None:
    """一行 → 领域事实；字段类型错位的行返回 None（= 丢弃）。

    SQLite 是**弱类型**：INTEGER 列里可以是 TEXT（外部写入 / 手工 sqlite3 / 未来版本
    改过列）。类型错位的行若被当成正常事实，一次减法就能让端点 500，或者让参照比较
    永远抛错、把这个会话的记账永久卡死。口径与旧实现逐字段归一同一套：坏行丢弃，
    不升级成整份失败。
    """
    ts, fp, verdict = row["ts"], row["fp"], row["verdict"]
    prompt, cached = _int(row["prompt"]), _int(row["cached"])
    if (
        not isinstance(ts, str)
        or not isinstance(fp, str)
        or not isinstance(verdict, str)
        or prompt is None
        or cached is None
    ):
        return None
    segment_count = _int(row["segment_count"])
    return CacheCall(
        ts=ts,
        fp=fp,
        purpose=_text(row["purpose"]),
        model=_text(row["model"]),
        provider=_text(row["provider"]),
        prompt=prompt,
        cached=cached,
        verdict=verdict,
        segment_count=segment_count if segment_count is not None else 0,
        ts_ms=_int(row["ts_ms"]),
        expected=_int(row["expected"]),
        ref_fp=row["ref_fp"] if isinstance(row["ref_fp"], str) else None,
        fork=_fork(row),
    )


def _text(value: Any) -> str:
    return value if isinstance(value, str) else ""


class CacheStore:
    """`cache_call` / `cache_segment` 的读写（会话维度，无缓存：每次现读库）。"""

    def __init__(self, session_id: str, database: Database | None = None) -> None:
        self._session_id = session_id
        # 连接在构造时**钉住**：一条账本操作（尤其是一笔事务）必须始终落在同一条连接
        # 上。每次现取 `get_database()` 的话，一次 `close_database()`（测试隔离 /
        # 进程收尾）会把后半段语句甩到另一条新连接上，事务直接裂开。
        self._database = database if database is not None else get_database()

    def database(self) -> Database:
        """本账本句柄钉住的那条连接（`database` 参数只给测试 / 显式装配用）。"""
        return self._database

    def transaction(self) -> AbstractContextManager[Transaction]:
        """把一次「读—改—写」放进同一个 `BEGIN IMMEDIATE`（见 `Database.transaction`）。

        等锁上限取短值（`_BUSY_TIMEOUT_MS`）：记账是 best-effort 的诊断数据，另一个
        进程攥着写锁时不该陪着等满 5 秒——那段时间本进程的锁是持着的，读路径会一起
        排队。放弃一笔账本记录远好过让所有会话的查询变慢。
        """
        return self.database().transaction(busy_timeout_ms=_BUSY_TIMEOUT_MS)

    # ── 读 ───────────────────────────────────────

    def recent(self, limit: int) -> list[CacheCall]:
        """最近 `limit` 条调用，**新→旧**（账本的完整事实面）。

        记账要的就是这一份：最近一条（分叉的对照侧）+ 参照候选（`prompt > 0` 的行，
        判「段列表是不是本次的前缀」由调用方做）。会话的行数上界是 `MAX_CALLS`，
        所以「整份读」在这个设计里最多几百行小表数据。
        """
        rows = self.database().query(_RECENT, (self._session_id, limit))
        return [call for call in map(_call, rows) if call is not None]

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
        return [part for part in map(_part, rows) if part is not None]

    def char_sums(self, fp: str, index: int) -> tuple[int, int]:
        """（段列表总字符数，前 `index` 段的字符数）——`tokens_lost` 的估算依据。"""
        rows = self.database().query(_CHAR_SUMS, (index, self._session_id, fp))
        if not rows:
            return 0, 0
        return int(_int(rows[0]["total"]) or 0), int(_int(rows[0]["before"]) or 0)

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
        """存一份段列表（同一指纹已经存过就跳过）。

        `fp` 只覆盖 `(label, sha)`——**不含 `src`**。文件被 touch 过（内容没变、mtime
        变了）时指纹不变，段落行因此保留首次落库的那份 `src`：`--why` 里的 mtime/size
        可能比磁盘上的当下事实旧一点（旧实现每次记账都覆盖，会显示最新值）。这是刻意
        的取舍——段列表按内容寻址，来源事实跟着「内容首次被记下的那一刻」。
        """
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

        淘汰与回收整体是一笔事务（嵌套进 `record` 的外层事务）：中途失败不会留下
        「调用没了、段落还在」的状态，也不会只删了一半段落行——丢一个孤儿的回收机会
        就没法再收集它了（下次裁剪按「即将消失的调用的 fp」算 victims，那些行已经不在
        调用表里）。

        Returns:
            本方法发出的语句是否都成功。没有需要淘汰的调用时也返回 ``True``：那不是
            失败。（是否**提交**由最外层事务说了算——嵌套时句柄还不是最终结果。）
        """
        with self.transaction() as txn:
            if not txn.live:
                return False
            victims = [
                row["fp"]
                for row in self.database().query(
                    _RING_VICTIMS, (self._session_id, keep)
                )
            ]
            if victims:
                ok = self.database().execute(_DELETE_RING, (self._session_id, keep))
                for fp in victims:
                    # `?1` / `?2` 复用：显式编号的占位符只按最大序号取参数
                    ok = (
                        self.database().execute(
                            _DELETE_ORPHAN_SEGMENTS, (self._session_id, fp)
                        )
                        and ok
                    )
                return ok
        return True

    def clear(self) -> bool:
        """删掉本会话在库里的全部行（同一个 id 被新会话接管时用）。

        Returns:
            写是否真的落库（库降级 / 回滚 → ``False``，但调用方不必为此报错）。
        """
        with self.transaction() as txn:
            if not txn.live:
                return False
            ok = self.database().execute(_DELETE_CALLS, (self._session_id,))
            ok = self.database().execute(_DELETE_SEGMENTS, (self._session_id,)) and ok
        return ok and txn.committed
