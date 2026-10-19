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


class WakeWriteLost(RuntimeError):
    """记账 / 状态写入没落库（``Database.execute`` 返回 false）。

    单独一个类型，是因为它和"没找到""已经终结"是**三种不同的结局**：把它折成
    ``False`` 会让 API 告诉用户"它早就完成了"，而库里那一行其实还在武装、到点照样投递。
    """

    def __init__(self, what: str) -> None:
        super().__init__(f"wing.db write lost: {what}")


def _placeholders(count: int) -> str:
    """``count`` 个逗号分隔的占位符（``IN (?, ?, …)``）。"""
    return ", ".join("?" for _ in range(count))


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
    ) -> bool:
        """改状态（终态）；``next_fire_at_ms`` 给定时一并改写排期。

        Returns:
            写是否真的落库（``Database.execute`` 的返回值）。**调用方必须看它**：
            写不进去意味着一行 ``active`` 触发器会继续被 ``due_timers`` 捞出来，
            调度器会把它当"还没触发过"再投一次。
        """
        assignments = ["state = ?"]
        params: list[Any] = [state]
        if state == STATE_FAILED:
            assignments.append("last_error = ?")
            params.append(error)
        if next_fire_at_ms is not None:
            assignments.append("next_fire_at_ms = ?")
            params.append(next_fire_at_ms)
        params.append(trigger_id)
        return self.database().execute(
            f"UPDATE wake_trigger SET {', '.join(assignments)} WHERE id = ?", params
        )

    def record_fire(
        self,
        trigger_id: int,
        *,
        fired_at_ms: int,
        decrement: int,
        next_fire_at_ms: int | None,
    ) -> bool:
        """一次**成功投递**之后的记账，全部在一条 SQL 里完成。

        字段：``fire_count + 1``、``last_fire_at_ms``、预算 ``- decrement``
        （``NULL`` = 无限则保持 ``NULL``）、下一次排期、以及"预算归零即终结"。

        为什么不先读再写：done 配额是**读—改—写**就必然有丢更新的窗口（两个并发分发
        基于同一份快照各扣一次，后写覆盖先写 → 配额永远到不了 0 → 整条触发器再也终结
        不了）。SQLite 的 SET 表达式读的是**旧行**，所以扣减 + 判终结 + 清排期同一条
        语句里做完是原子的；``WHERE state = 'active'`` 只保证**终态行不会被改写**
        （它**不是**认领条件：``Database.execute`` 不看 rowcount，猜不到是谁写的，
        跨进程双投递的防线只有"一台机器一个网关"这条部署假设）。

        Args:
            decrement: 本次扣多少（done 事件 1，ask 事件 0）。
            next_fire_at_ms: 未终结时的下一次排期（session 型传 ``None``）。

        Returns:
            写是否真的落库。
        """
        return self.database().execute(
            """
            UPDATE wake_trigger
               SET fire_count = fire_count + 1,
                   last_fire_at_ms = ?,
                   remaining_done = CASE
                       WHEN remaining_done IS NULL THEN NULL
                       ELSE remaining_done - ? END,
                   next_fire_at_ms = CASE
                       WHEN remaining_done IS NOT NULL AND remaining_done - ? <= 0
                       THEN NULL ELSE ? END,
                   state = CASE
                       WHEN remaining_done IS NOT NULL AND remaining_done - ? <= 0
                       THEN 'done' ELSE 'active' END
             WHERE id = ? AND state = 'active'
            """,
            (
                fired_at_ms,
                decrement,
                decrement,
                next_fire_at_ms,
                decrement,
                trigger_id,
            ),
        )

    def reschedule(self, trigger_id: int, next_fire_at_ms: int) -> bool:
        """只改下一次排期（周期型重启对齐）。返回写是否落库。"""
        return self.database().execute(
            "UPDATE wake_trigger SET next_fire_at_ms = ? WHERE id = ?",
            (next_fire_at_ms, trigger_id),
        )

    def cancel(self, trigger_id: int) -> bool | None:
        """取消一条 ``active`` 触发器。

        Returns:
            ``True`` = 本次取消生效；``False`` = 存在但已终结（幂等 no-op）；
            ``None`` = 该 id 不存在（路由据此回 404）。

        Raises:
            WakeWriteLost: 状态写没落库（调用方要如实报"取消没生效"，不能说"它早完了"）
        """
        db = self.database()
        rows = db.query("SELECT state FROM wake_trigger WHERE id = ?", (trigger_id,))
        if not rows:
            return None
        if str(rows[0]["state"]) != STATE_ACTIVE:
            return False
        if not db.execute(
            "UPDATE wake_trigger SET state = ? WHERE id = ? AND state = ?",
            (STATE_CANCELED, trigger_id, STATE_ACTIVE),
        ):
            # **写丢失 ≠ 早就终结**：两者都让"取消没发生"，但原因不同，调用方
            # 要能对用户说出是哪一种（`False` 会被读成"它本来就完了"）。
            raise WakeWriteLost(f"cancel of wake #{trigger_id}")
        return True

    def cancel_for_session(self, session_id: str) -> tuple[int, int]:
        """把 source 或 target 命中该会话的 ``active`` 触发器全部取消。

        Returns:
            ``(written, matched)``：真的写成的条数、命中的条数。两者不同就是**写丢失**
            （不是"本来就没有"），调用方据此留一行 WARN。

        写丢失时**不抛**：这条路径挂在归档上，归档本身不能因为唤醒记账失败而失败。
        """
        db = self.database()
        rows = db.query(
            "SELECT id FROM wake_trigger WHERE state = ? "
            "AND (source_session_id = ? OR target_session_id = ?)",
            (STATE_ACTIVE, session_id, session_id),
        )
        ids = [int(row["id"]) for row in rows]
        if not ids:
            return 0, 0
        written = db.executemany(
            "UPDATE wake_trigger SET state = ? WHERE id = ? AND state = ?",
            [(STATE_CANCELED, trigger_id, STATE_ACTIVE) for trigger_id in ids],
        )
        return (len(ids) if written else 0), len(ids)

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

    def due_timers(
        self, now_ms: int, *, exclude: frozenset[int] = frozenset()
    ) -> list[WakeTrigger]:
        """所有已到期的 ``active`` timer（按排期、再按 id 定序——同一 tick 内顺序确定）。

        ``exclude`` 是本进程**暂不重试**的 id（记账写失败的那些，见
        ``WakeService._stall``）：它们状态上仍是 ``active``，但再捞出来只会再投一次。
        """
        sql = (
            f"SELECT {_COLUMNS} FROM wake_trigger "
            "WHERE state = ? AND kind = 'timer' "
            "AND next_fire_at_ms IS NOT NULL AND next_fire_at_ms <= ?"
        )
        params: list[Any] = [STATE_ACTIVE, now_ms]
        if exclude:
            sql += f" AND id NOT IN ({_placeholders(len(exclude))})"
            params.extend(sorted(exclude))
        sql += " ORDER BY next_fire_at_ms, id"
        return [WakeTrigger.from_row(row) for row in self.database().query(sql, params)]

    def next_due_ms(self, *, exclude: frozenset[int] = frozenset()) -> int | None:
        """最近一条 ``active`` timer 的排期（无则 ``None``——调度器据此无限等）。"""
        sql = (
            "SELECT MIN(next_fire_at_ms) AS next FROM wake_trigger "
            "WHERE state = ? AND kind = 'timer' AND next_fire_at_ms IS NOT NULL"
        )
        params: list[Any] = [STATE_ACTIVE]
        if exclude:
            sql += f" AND id NOT IN ({_placeholders(len(exclude))})"
            params.extend(sorted(exclude))
        rows = self.database().query(sql, params)
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
    "WakeWriteLost",
]
