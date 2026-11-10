# wing/metrics/store.py — 用量聚合的唯一 SQL 处（``audit_*`` + ``session_catalog``）
"""``/api/usage`` 的读侧仓储：常量 SQL + 位置绑定 + 行→领域对象的容错。

三条口径决定了这里的 SQL 长成现在这样：

- **「一天」按写盘机的本地日历日**：``substr(ts, 1, 10)``（``ts`` 是
  ``datetime.now().isoformat()`` 冻结下来的本地墙钟）。``date(ts_ms, 'unixepoch',
  'localtime')`` 随**读端** ``TZ`` 漂移，因此不用。
- **窗口过滤走双谓词**：``ts`` 文本判定归属（精确、与分桶同口径），``ts_ms`` 给
  索引一个略宽的包络（``idx_audit_*_ts`` 的范围扫描）。两者不是同一种投影，读写
  两端时区不同时窄的 ``ts_ms`` 边界会切掉边缘行，所以包络故意放宽（见
  ``_TZ_GUARD_DAYS``）。
- **三张表先摊平成「每行一条贡献向量」再聚合**（``_contributions``）：
  ``sessions`` 是「桶内有 ≥1 条审计事实的不同会话数」，跨表去重只能在同一条流上
  做 ``COUNT(DISTINCT session_id)``——三张表各自的 ``COUNT(DISTINCT)`` 相加会把
  「既调了模型又用了工具」的会话算两遍。

不判断 ``Database.ready``：库不可用时 ``query`` 返回空列表，聚合因此全零（降级语义
只在 ``wing.db`` 一处，与 ``audit/recorder.py`` / ``cache_store.py`` 同口径）。
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from datetime import datetime, time, timedelta
from typing import Any

from wing.db import DIAGNOSTIC_BUSY_TIMEOUT_MS, Database, get_database

from .measures import MEASURE_FIELDS, Measures, Window

#: ``ts_ms`` 包络相对本地日期边界放宽的天数。上下界各放宽一次，覆盖读写两端
#: 时区相差（含 DST）的全部合法情况；只影响扫描范围，归属仍由 ``ts`` 判定。
_TZ_GUARD_DAYS = 2

_SCOPE_ONE = " AND session_id = ?"

#: 子孙闭包（``UNION`` 而非 ``UNION ALL``：血缘里出现环时递归自然终止）。
#: 写成子查询而不是整条语句的 ``WITH`` 前缀，是为了让范围参数仍排在最前。
_SCOPE_TREE = (
    " AND session_id IN (WITH RECURSIVE tree(sid) AS ("
    "SELECT ? UNION SELECT c.session_id FROM session_catalog c "
    "JOIN tree ON c.parent_session_id = tree.sid) SELECT sid FROM tree)"
)

#: 摊平流里的**可加列**：度量词表去掉 ``sessions``（去重计数，不能相加）。
_ADDITIVE_COLUMNS: tuple[str, ...] = tuple(
    name for name in MEASURE_FIELDS if name != "sessions"
)


def _measure_columns(*, not_attributable: tuple[str, ...] = ()) -> str:
    """聚合列清单（列名 == 度量名）：``sessions`` 去重计数，其余求和。

    ``not_attributable`` 点名的度量在该维度没有事实来源（工具行没有 model 列、
    模型行没有工具名列），一律 ``0 AS <name>`` —— 0 在这里的含义是「不可归因」，
    不是「测得 0」。词表只有 :data:`MEASURE_FIELDS` 一处声明，维度差异只在这里
    以声明的方式出现（可归因性表见 docs/dev/usage.md）。
    """
    parts = [
        "0 AS sessions"
        if "sessions" in not_attributable
        else "COUNT(DISTINCT session_id) AS sessions"
    ]
    for name in _ADDITIVE_COLUMNS:
        parts.append(
            f"0 AS {name}" if name in not_attributable else f"SUM({name}) AS {name}"
        )
    return ", ".join(parts)


#: 三张审计表摊平后的**全部**度量都可归因（会话 / 天 / 归属 / workspace / 模板）。
_MEASURE_COLUMNS = _measure_columns()

#: 整段窗口的总量：在摊平流上直接聚合（无 GROUP BY）。
_TOTALS = f"SELECT {_MEASURE_COLUMNS} FROM ({{contributions}})"

#: 按天：``day`` 来自摊平流，稠密化由 ``report`` 负责（SQL 只出有数据的天）。
_BY_DAY = f"SELECT day, {_MEASURE_COLUMNS} FROM ({{contributions}}) GROUP BY day"

#: 按会话：一次拿到 7 个维度里 4 个（session / origin / workspace / agent）的全部素材。
_BY_SESSION = f"""
SELECT b.*, cat.parent_session_id AS parent_session_id,
       cat.workspace AS workspace, cat.template_name AS template_name
FROM (SELECT session_id, {_MEASURE_COLUMNS} FROM ({{contributions}}) GROUP BY session_id) b
LEFT JOIN session_catalog cat ON cat.session_id = b.session_id
"""

#: 模型维度的**单行贡献**：只有 LLM 侧列（工具与用户消息行没有 model 列）。
_MODEL_CONTRIBUTIONS = """
SELECT COALESCE(provider, '') AS provider, model AS model,
       session_id AS session_id, 1 AS llm_calls,
       prompt_tokens AS prompt_tokens, completion_tokens AS completion_tokens,
       cached_tokens AS cached_tokens, COALESCE(duration_ms, 0) AS llm_time_ms
FROM audit_llm_call WHERE {audit}
"""

#: 工具维度的**单行贡献**：只有工具侧列。
_TOOL_CONTRIBUTIONS = """
SELECT tool_name AS tool_name, session_id AS session_id, 1 AS tool_calls,
       CASE WHEN success = 0 THEN 1 ELSE 0 END AS tool_errors,
       COALESCE(duration_ms, 0) AS tool_time_ms
FROM audit_tool_call WHERE {audit}
"""

#: 不可归因的度量：工具 / 用户消息行没有 model 列，模型行没有工具名列。
_NOT_LLM_MEASURES = ("tool_calls", "tool_errors", "tool_time_ms", "user_messages")
_NOT_TOOL_MEASURES = (
    "llm_calls",
    "prompt_tokens",
    "completion_tokens",
    "cached_tokens",
    "llm_time_ms",
    "user_messages",
)

_BY_MODEL = f"""
SELECT provider, model,
       {_measure_columns(not_attributable=_NOT_LLM_MEASURES)}
FROM ({_MODEL_CONTRIBUTIONS}) GROUP BY provider, model
"""

_BY_TOOL = f"""
SELECT tool_name,
       {_measure_columns(not_attributable=_NOT_TOOL_MEASURES)}
FROM ({_TOOL_CONTRIBUTIONS}) GROUP BY tool_name
"""

#: 「有记录的第一天」= 审计流水（三张 ``audit_*``）最早的一天；无审计行 → NULL。
#:
#: 只看流水：``session_catalog`` 是为**归属**（血缘 / workspace / 模板）而写的派生投影，
#: 它不能把窗口往前拉——早于流水的那段时间没有任何读数（不是 0）。
#:
#: 每个子查询走 ``ORDER BY ts_ms LIMIT 1``（``idx_audit_*_ts`` 上的索引扫描）而不是
#: ``MIN(substr(ts, 1, 10))``：后者是每张表的全扫描，且这条路径**每个** ``/api/usage``
#: 请求都要走一次（实测 100 万行 44ms vs 0.0ms）。``ts`` 与 ``ts_ms`` 同源同刻，
#: 取最早一刻的那一行得到的日期就是最早的一天。
_EARLIEST_DAY = (
    "SELECT MIN(d) AS day FROM ("
    "SELECT (SELECT substr(ts, 1, 10) FROM audit_llm_call"
    " ORDER BY ts_ms LIMIT 1) AS d"
    " UNION ALL SELECT (SELECT substr(ts, 1, 10) FROM audit_tool_call"
    " ORDER BY ts_ms LIMIT 1)"
    " UNION ALL SELECT (SELECT substr(ts, 1, 10) FROM audit_user_message"
    " ORDER BY ts_ms LIMIT 1))"
)

#: 会话作用域的 404 判据 = 「这几张表里有没有它的任何一行」。表清单是唯一事实：
#: 绑定参数的个数由它派生（漏一张表就会静默把所有带会话的请求判成 404）。
_SCOPE_TABLES: tuple[str, ...] = (
    "session_catalog",
    "audit_llm_call",
    "audit_tool_call",
    "audit_user_message",
)

_SCOPE_EXISTS = "SELECT 1 FROM ({}) LIMIT 1".format(
    " UNION ALL ".join(
        f"SELECT 1 FROM {table} WHERE session_id = ?" for table in _SCOPE_TABLES
    )
)


@dataclass(frozen=True, slots=True)
class SessionBucket:
    """per-session 聚合的一行：会话 + 它的度量 + 目录事实。"""

    session_id: str
    measures: Measures
    parent_session_id: str | None
    workspace: str | None
    template_name: str | None


@dataclass(frozen=True, slots=True)
class ModelBucket:
    """``audit_llm_call`` 的一个 ``(provider, model)`` 桶。"""

    provider: str
    model: str
    measures: Measures


@dataclass(frozen=True, slots=True)
class ToolBucket:
    """``audit_tool_call`` 的一个工具桶。"""

    tool_name: str
    measures: Measures


def _count(value: Any) -> int:
    """``COUNT`` / ``SUM`` 的取值：真数值才算数（``bool`` 不是计数，NULL / 怪类型按 0）。

    与 ``cache_store._is_int`` 的差别只在 ``float``：那边读的是**存储列**（整数列里
    混进 REAL 就是坏行），这里读的是 ``SUM()`` 的结果——弱类型列里藏一个 REAL 会让
    整桶求和多出小数位，按 0 丢掉整桶比丢掉那个小数位更糟。
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0
    return int(value)


def _text(value: Any) -> str | None:
    return value if isinstance(value, str) else None


def _measures(row: sqlite3.Row) -> Measures:
    """一行聚合 → :class:`Measures`（列名即字段名，见 ``_MEASURE_COLUMNS``）。

    键集取自 :data:`MEASURE_FIELDS`（加一个度量只改词表）。
    """
    return Measures(**{name: _count(row[name]) for name in MEASURE_FIELDS})


def _ms_envelope(window: Window) -> tuple[int, int]:
    """``ts_ms`` 的宽松包络（本地日期边界 ± ``_TZ_GUARD_DAYS``）。"""
    low = datetime.combine(window.since, time.min) - timedelta(days=_TZ_GUARD_DAYS)
    high = datetime.combine(window.until + timedelta(days=1), time.min) + timedelta(
        days=_TZ_GUARD_DAYS
    )
    return int(low.timestamp() * 1000), int(high.timestamp() * 1000)


def _scope_predicate(window: Window) -> tuple[str, list[Any]]:
    """会话作用域谓词（无作用域 = 空串；``tree`` 时是递归闭包）。"""
    if window.session_id is None:
        return "", []
    return (_SCOPE_TREE if window.tree else _SCOPE_ONE), [window.session_id]


def _audit_predicate(window: Window) -> tuple[str, list[Any]]:
    """三张审计表的 ``WHERE`` 主体（列名三表一致：``ts`` / ``ts_ms`` / ``session_id``）。

    不筛 ``session_id IS NOT NULL``：无会话的调用行仍是真实调用（该进 ``llm_calls``
    与 token），只是进不了任何会话桶——``COUNT(DISTINCT session_id)`` 本来就忽略
    NULL，``by_session`` 再丢掉这类分组。
    """
    since, until = window.day_strings()
    low, high = _ms_envelope(window)
    sql = "ts >= ? AND ts < ? AND ts_ms >= ? AND ts_ms < ?"
    params: list[Any] = [
        since,
        (window.until + timedelta(days=1)).isoformat(),
        low,
        high,
    ]
    scope, scope_params = _scope_predicate(window)
    return sql + scope, params + scope_params


def _contributions(window: Window) -> tuple[str, list[Any]]:
    """三张审计表 → 每行一条贡献向量（列名统一成度量名）的 ``UNION ALL``。

    摊平是同一条流上 ``COUNT(DISTINCT session_id)`` 的前提：``sessions`` 必须跨表
    去重（同一会话既调模型又用工具时，两次 ``COUNT(DISTINCT)`` 相加会翻倍）。
    """
    where, params = _audit_predicate(window)
    sql = (
        "SELECT session_id AS session_id, substr(ts, 1, 10) AS day,"
        " 1 AS llm_calls, 0 AS tool_calls, 0 AS tool_errors, 0 AS user_messages,"
        " prompt_tokens AS prompt_tokens, completion_tokens AS completion_tokens,"
        " cached_tokens AS cached_tokens, COALESCE(duration_ms, 0) AS llm_time_ms,"
        " 0 AS tool_time_ms"
        f" FROM audit_llm_call WHERE {where}"
        " UNION ALL"
        " SELECT session_id, substr(ts, 1, 10), 0, 1,"
        " CASE WHEN success = 0 THEN 1 ELSE 0 END, 0, 0, 0, 0, 0,"
        " COALESCE(duration_ms, 0)"
        f" FROM audit_tool_call WHERE {where}"
        " UNION ALL"
        " SELECT session_id, substr(ts, 1, 10), 0, 0, 0, 1, 0, 0, 0, 0, 0"
        f" FROM audit_user_message WHERE {where}"
    )
    return sql, params * 3


class MetricsStore:
    """``audit_*`` / ``session_catalog`` 的只读聚合（每次现读库，无缓存）。"""

    def __init__(self, database: Database | None = None) -> None:
        self._database = database if database is not None else get_database()

    def database(self) -> Database:
        """本仓储读的那条连接（``database`` 参数只给测试 / 显式装配用）。"""
        return self._database

    def is_available(self) -> bool:
        """库可用吗（``wing.db`` 打不开时整条读路径降级为全零）。

        404 判据要用它：库降级时「读不到」不等于「不存在」，不能据此回 ``not_found``。
        """
        return self._database.ready

    def _rows(self, sql: str, params: list[Any] | tuple[Any, ...]) -> list[sqlite3.Row]:
        return self.database().query(
            sql, params, busy_timeout_ms=DIAGNOSTIC_BUSY_TIMEOUT_MS
        )

    # ── 读 ───────────────────────────────────────

    def totals(self, window: Window) -> Measures:
        """整段窗口的总量。"""
        sql, params = _contributions(window)
        rows = self._rows(_TOTALS.format(contributions=sql), params)
        return _measures(rows[0]) if rows else Measures()

    def by_day(self, window: Window) -> dict[str, Measures]:
        """按天聚合（只含有数据的天；稠密化归 ``report``）。"""
        sql, params = _contributions(window)
        return {
            day: _measures(row)
            for row in self._rows(_BY_DAY.format(contributions=sql), params)
            if (day := _text(row["day"])) is not None
        }

    def by_session(self, window: Window) -> list[SessionBucket]:
        """按会话聚合（脏行丢弃：``session_id`` 不是文本就无法归因）。"""
        sql, params = _contributions(window)
        buckets: list[SessionBucket] = []
        for row in self._rows(_BY_SESSION.format(contributions=sql), params):
            session_id = _text(row["session_id"])
            if session_id is None:
                continue
            buckets.append(
                SessionBucket(
                    session_id=session_id,
                    measures=_measures(row),
                    parent_session_id=_text(row["parent_session_id"]),
                    workspace=_text(row["workspace"]),
                    template_name=_text(row["template_name"]),
                )
            )
        return buckets

    def by_model(self, window: Window) -> list[ModelBucket]:
        """按 ``(provider, model)`` 聚合（``model`` 是调用名，provider 是承载它的那个）。"""
        where, params = _audit_predicate(window)
        buckets: list[ModelBucket] = []
        for row in self._rows(_BY_MODEL.format(audit=where), params):
            model = _text(row["model"])
            if model is None:
                continue
            buckets.append(
                ModelBucket(
                    provider=_text(row["provider"]) or "",
                    model=model,
                    measures=_measures(row),
                )
            )
        return buckets

    def by_tool(self, window: Window) -> list[ToolBucket]:
        """按工具名聚合。"""
        where, params = _audit_predicate(window)
        buckets: list[ToolBucket] = []
        for row in self._rows(_BY_TOOL.format(audit=where), params):
            tool_name = _text(row["tool_name"])
            if tool_name is None:
                continue
            buckets.append(ToolBucket(tool_name=tool_name, measures=_measures(row)))
        return buckets

    def earliest_day(self) -> str | None:
        """有记录的第一天（三张 ``audit_*`` 的 ``MIN``；库为空 → ``None``）。"""
        rows = self._rows(_EARLIEST_DAY, ())
        return _text(rows[0]["day"]) if rows else None

    def has_usage_record(self, session_id: str) -> bool:
        """该 id 在统计口径上是否有任何痕迹（目录行**或**审计行）—— 404 判据。

        与 ``SessionManager.session_exists`` **不是同一个判据**：那边回答「这个会话
        现在能不能用」（内存 + 各 store + id 形状），这里回答「统计认识这个 id 吗」
        （只读库、不水合）。两边在归档会话与空会话上故意给出不同答案。
        """
        rows = self._rows(_SCOPE_EXISTS, (session_id,) * len(_SCOPE_TABLES))
        return bool(rows)
