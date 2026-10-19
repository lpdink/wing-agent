"""wing/audit/recorder.py — 审计记录器：事件 → ``audit_*`` 表的追加行。

一条事件一次 INSERT（同步、自动提交、微秒级），不再有「读整份 JSON → 聚合 → 重写」
的写放大。记录器只做分发与取值，落库语义（降级 / 迁移）全在 ``wing.db``。

不变量：
  - ``ToolCallResultEvent.duration_ms is None`` = 未真实执行（中断合成 / 参数非法 /
    未知工具短路），**不写行**——与会话用量账本 ``UsageTracker.record_tool_call``
    同口径：那不是一次执行。
  - 无 ``session_id`` 的压缩事件不写行（``audit_compaction.session_id`` 是 NOT NULL，
    压缩必属于某个会话）。
  - 任何异常都在本层兜住（``wing.db`` 已降级 + 这里第二道网）：审计失败绝不影响 agent。
"""

from __future__ import annotations

from datetime import datetime

from wing.common.logger import log
from wing.db import Database, get_database
from wing.event import (
    CompactDoneEvent,
    LLMCallMetricsEvent,
    ToolCallResultEvent,
    WingEvent,
)

_INSERT_LLM_CALL = """
INSERT INTO audit_llm_call (
    ts_ms, ts, session_id, request_id, model, provider,
    prompt_tokens, completion_tokens, cached_tokens,
    first_chunk_rt_ms, tokens_per_sec, duration_ms, stop_reason
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

_INSERT_TOOL_CALL = """
INSERT INTO audit_tool_call (
    ts_ms, ts, session_id, request_id, tool_name, tool_call_id, success, duration_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
"""

_INSERT_COMPACTION = """
INSERT INTO audit_compaction (
    ts_ms, ts, session_id, model, original_tokens, compressed_tokens
) VALUES (?, ?, ?, ?, ?, ?)
"""


def _stamp(event: WingEvent) -> tuple[int, str]:
    """事件的创建时刻：``(epoch 毫秒, 本地 naive ISO)``。

    取 ``created_at`` 而不是插入时刻——事件创建才是事实发生的时刻（队列可能有延迟），
    ISO 口径与 ``last_turn_end`` / 链记录的 ``ts`` 一致（``datetime.now().isoformat()``）。
    """
    created: datetime = event.created_at
    return int(created.timestamp() * 1000), created.isoformat()


class AuditRecorder:
    """EventBus 订阅者：按事件类型把一行行事实写进 ``wing.db``。

    ``database`` 缺省时每条事件现取进程单例 ``get_database()``——``reset_database()``
    之后的状态切换对已订阅的记录器透明（测试隔离依赖这一点）。
    """

    def __init__(self, database: Database | None = None) -> None:
        self._database = database

    def on_event(self, event: WingEvent) -> None:
        try:
            if isinstance(event, LLMCallMetricsEvent):
                self._record_llm_call(event)
            elif isinstance(event, ToolCallResultEvent):
                self._record_tool_call(event)
            elif isinstance(event, CompactDoneEvent):
                self._record_compaction(event)
        except Exception as exc:  # 兜底：审计失败不影响 agent
            log.error(f"audit: failed to record {type(event).__name__}: {exc!r}")

    # ── 三张表 ───────────────────────────────────

    def _record_llm_call(self, event: LLMCallMetricsEvent) -> None:
        ts_ms, ts = _stamp(event)
        self._db().execute(
            _INSERT_LLM_CALL,
            (
                ts_ms,
                ts,
                event.session_id,
                event.request_id,
                event.model,
                event.provider,
                event.prompt_tokens,
                event.completion_tokens,
                event.cached_tokens,
                event.first_chunk_rt_ms,
                event.tokens_per_sec,
                event.duration_ms,
                event.stop_reason,
            ),
        )

    def _record_tool_call(self, event: ToolCallResultEvent) -> None:
        if event.duration_ms is None:
            return
        ts_ms, ts = _stamp(event)
        self._db().execute(
            _INSERT_TOOL_CALL,
            (
                ts_ms,
                ts,
                event.session_id,
                event.request_id,
                event.tool_name,
                event.tool_call_id,
                1 if event.tool_success else 0,
                event.duration_ms,
            ),
        )

    def _record_compaction(self, event: CompactDoneEvent) -> None:
        if not event.session_id:
            return
        ts_ms, ts = _stamp(event)
        self._db().execute(
            _INSERT_COMPACTION,
            (
                ts_ms,
                ts,
                event.session_id,
                event.model,
                event.original_tokens,
                event.compressed_tokens,
            ),
        )

    # ── 内部 ─────────────────────────────────────

    def _db(self) -> Database:
        return self._database if self._database is not None else get_database()
