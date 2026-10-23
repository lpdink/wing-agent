# wing/metrics — 用量统计：审计流水 + 会话目录的只读聚合
"""``GET /api/usage`` 的领域层：把 ``audit_*`` 流水与 ``session_catalog`` 目录投影成
「同一组度量 × 一个维度」。

分工：

- :mod:`wing.metrics.store` 是**唯一**握有这两组表 SQL 的地方（常量 SQL + 位置绑定 +
  坏行丢弃）；
- :mod:`wing.metrics.measures` 是度量的唯一词表（11 个整数 + 两个值类型，纯函数）；
- :mod:`wing.metrics.report` 是查询编排（窗口 → 各维度的行 → 线格式），不写 SQL、
  不 import ``gateway``；
- :mod:`wing.metrics.catalog` 是**写侧**：会话目录行的采集与启动回填（目录是派生投影，
  磁盘才是事实源）。

写路径的其余两处不在本包：用户消息行由 ``audit/recorder.py`` 随事件写（与既有三张
``audit_*`` 同一条管道），会话目录行由 ``SessionManager`` 在新建分支上调用
:func:`wing.metrics.catalog.record_session_created`。
"""

from __future__ import annotations

from wing.metrics.catalog import backfill_catalog, record_session_created
from wing.metrics.measures import MEASURE_FIELDS, Measures, Row, Window, sum_measures
from wing.metrics.report import (
    GROUP_BY_VALUES,
    MAX_LIMIT,
    ORIGIN_VALUES,
    usage_report,
)
from wing.metrics.store import MetricsStore

__all__ = [
    "GROUP_BY_VALUES",
    "MAX_LIMIT",
    "MEASURE_FIELDS",
    "ORIGIN_VALUES",
    "Measures",
    "MetricsStore",
    "Row",
    "Window",
    "backfill_catalog",
    "record_session_created",
    "sum_measures",
    "usage_report",
]
