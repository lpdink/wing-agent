"""wing/wake/store.py — ``wake_trigger`` 的持久化（唯一 SQL 处）。

走进程级 ``wing.db``（``get_database()``）：DB 降级时 ``Database`` 的读写作 no-op，这里的每个方法
因此天然是「空操作或空结果」——**本层不重复判断 ready**，降级语义只在一个地方（``wing.db``）。

id 由 SQLite 分配（``INTEGER PRIMARY KEY`` = rowid）；取回用同一条连接上的
``last_insert_rowid()``——连接单条、只在事件循环线程使用（``Database`` 的契约），插入与取回之间
不可能被别的写插入。不改 ``Database`` 的公共面。
"""

from __future__ import annotations

from collections.abc import Sequence
from typing import Any

from wing.db import Database, get_database
from wing.wake.model import (
    STATE_ACTIVE,
    STATE_CANCELED,
    STATE_DONE,
    STATE_FAILED,
    WakeTrigger,
)

_INSERT = """
INSERT INTO wake_trigger (
    kind, target_session_id, source_session_id, on_events, spec,
    next_fire_at_ms, remaining_done, prompt, state, fire_count,
    last_fire_at_ms, last_error, created_at_ms, created_by
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

_COLUMNS = "*"


class WakeStore:
    """``wake_trigger`` 的 CRUD（无缓存：触发器是低频数据，每次现读库）。"""

    def __init__(self, database: Database | None = None) -> None:
        self._database = database

    # ── 写 ───────────────────────────────────────

    def insert(
        self,
        *,
        kind: str,
        target_session_id: str,
        source_session_id: str | None,
        on_events: str,
        spec: str | None,
        next_fire_at_ms: int | None,
        remaining_done: int | None,
        prompt: str | None,
        created_at_ms: int,
        created_by: str | None,
    ) -> WakeTrigger | None:
        """插入一条 ``active`` 触发器并读回；DB 不可用返回 ``None``。"""
        db = self.database()
        ok = db.execute(
            _INSERT,
            (
                kind,
                target_session_id,
                source_session_id,
                on_events,
                spec,
                next_fire_at_ms,
                remaining_done,
                prompt,
                STATE_ACTIVE,
                0,
                None,
                None,
                created_at_ms,
                created_by,
            ),
        )
        if not ok:
            return None
        rows = db.query("SELECT last_insert_rowid() AS id")
        if not rows:
            return None
        return self.get(int(rows[0]["id"]))

    def set_state(
        self,
        trigger_id: int,
        state: str,
        *,
        error: str | None = None,
        next_fire_at_ms: int | None = None,
    ) -> None:
        """改状态（终态）；``next_fire_at_ms`` 给定时一并改写排期。"""
        assignments = ["state = ?"]
        params: list[Any] = [state]
        if state == STATE_FAILED:
            assignments.append("last_error = ?")
            params.append(error)
        if next_fire_at_ms is not None:
            assignments.append("next_fire_at_ms = ?")
            params.append(next_fire_at_ms)
        params.append(trigger_id)
        self.database().execute(
            f"UPDATE wake_trigger SET {', '.join(assignments)} WHERE id = ?", params
        )

    def mark_fired(
        self,
        trigger_id: int,
        *,
        fired_at_ms: int,
        next_fire_at_ms: int | None,
        remaining_done: int | None,
        state: str,
    ) -> None:
        """一次**成功投递**之后的记账：fire_count + 1、last_fire_at、预算、下一次排期、状态。"""
        self.database().execute(
            """
            UPDATE wake_trigger
               SET fire_count = fire_count + 1,
                   last_fire_at_ms = ?,
                   next_fire_at_ms = ?,
                   remaining_done = ?,
                   state = ?
             WHERE id = ?
            """,
            (fired_at_ms, next_fire_at_ms, remaining_done, state, trigger_id),
        )

    def reschedule(self, trigger_id: int, next_fire_at_ms: int) -> None:
        """只改下一次排期（周期型重启对齐）。"""
        self.database().execute(
            "UPDATE wake_trigger SET next_fire_at_ms = ? WHERE id = ?",
            (next_fire_at_ms, trigger_id),
        )

    def cancel(self, trigger_id: int) -> bool | None:
        """取消一条 ``active`` 触发器。

        Returns:
            ``True`` = 本次取消生效；``False`` = 存在但已终结（幂等 no-op）；
            ``None`` = 该 id 不存在（路由据此回 404）。
        """
        db = self.database()
        rows = db.query("SELECT state FROM wake_trigger WHERE id = ?", (trigger_id,))
        if not rows:
            return None
        if str(rows[0]["state"]) != STATE_ACTIVE:
            return False
        db.execute(
            "UPDATE wake_trigger SET state = ? WHERE id = ? AND state = ?",
            (STATE_CANCELED, trigger_id, STATE_ACTIVE),
        )
        return True

    def cancel_for_session(self, session_id: str) -> int:
        """把 source 或 target 命中该会话的 ``active`` 触发器全部取消，返回条数。"""
        db = self.database()
        rows = db.query(
            "SELECT id FROM wake_trigger WHERE state = ? "
            "AND (source_session_id = ? OR target_session_id = ?)",
            (STATE_ACTIVE, session_id, session_id),
        )
        ids = [int(row["id"]) for row in rows]
        if not ids:
            return 0
        db.executemany(
            "UPDATE wake_trigger SET state = ? WHERE id = ? AND state = ?",
            [(STATE_CANCELED, trigger_id, STATE_ACTIVE) for trigger_id in ids],
        )
        return len(ids)

    # ── 读 ───────────────────────────────────────

    def get(self, trigger_id: int) -> WakeTrigger | None:
        rows = self.database().query(
            f"SELECT {_COLUMNS} FROM wake_trigger WHERE id = ?", (trigger_id,)
        )
        return WakeTrigger.from_row(rows[0]) if rows else None

    def list_triggers(self, *, include_finished: bool = False) -> list[WakeTrigger]:
        """列出触发器（``include_finished`` 为假只看 ``active``；新的在前）。"""
        sql = f"SELECT {_COLUMNS} FROM wake_trigger"
        params: Sequence[Any] = ()
        if not include_finished:
            sql += " WHERE state = ?"
            params = (STATE_ACTIVE,)
        sql += " ORDER BY id DESC"
        return [WakeTrigger.from_row(row) for row in self.database().query(sql, params)]

    def active_for_source(self, session_id: str) -> list[WakeTrigger]:
        rows = self.database().query(
            f"SELECT {_COLUMNS} FROM wake_trigger "
            "WHERE source_session_id = ? AND state = ? ORDER BY id",
            (session_id, STATE_ACTIVE),
        )
        return [WakeTrigger.from_row(row) for row in rows]

    def due_timers(self, now_ms: int) -> list[WakeTrigger]:
        """所有已到期的 ``active`` timer（按排期、再按 id 定序——同一 tick 内顺序确定）。"""
        rows = self.database().query(
            f"SELECT {_COLUMNS} FROM wake_trigger "
            "WHERE state = ? AND kind = 'timer' "
            "AND next_fire_at_ms IS NOT NULL AND next_fire_at_ms <= ? "
            "ORDER BY next_fire_at_ms, id",
            (STATE_ACTIVE, now_ms),
        )
        return [WakeTrigger.from_row(row) for row in rows]

    def next_due_ms(self) -> int | None:
        """最近一条 ``active`` timer 的排期（无则 ``None``——调度器据此无限等）。"""
        rows = self.database().query(
            "SELECT MIN(next_fire_at_ms) AS next FROM wake_trigger "
            "WHERE state = ? AND kind = 'timer' AND next_fire_at_ms IS NOT NULL",
            (STATE_ACTIVE,),
        )
        if not rows:
            return None
        value = rows[0]["next"]
        return int(value) if value is not None else None

    # ── 内部 ─────────────────────────────────────

    def database(self) -> Database:
        """本 store 写入的库（缺省 = 进程单例；测试可注入临时库）。"""
        return self._database if self._database is not None else get_database()


#: 状态常量再导出（service 用同一套字面量，避免全包散落字符串）。
__all__ = [
    "STATE_ACTIVE",
    "STATE_CANCELED",
    "STATE_DONE",
    "STATE_FAILED",
    "WakeStore",
]
