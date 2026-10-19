"""tests/test_audit_recorder.py — 审计：事件 → wing.db 的三张表。

覆盖：
  - 三类事件各写一行，字段逐一断言（含 provider / success / 时间戳口径）；
  - `duration_ms is None` 的工具结果不写行（中断合成结果不是一次执行）；
  - 无 session_id 的压缩不写行；
  - DB 降级时 on_event 不抛（审计不能把 agent 带崩）；
  - `install()` 幂等：重复安装不会双写；
  - 全链路：EventBus.emit → recorder → `<WING_HOME>/core/wing.db` 的行。
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from wing.audit import install
from wing.audit.recorder import AuditRecorder
from wing.db import get_database
from wing.db.database import Database
from wing.event import (
    CompactDoneEvent,
    LLMCallMetricsEvent,
    TextEvent,
    ToolCallResultEvent,
)
from wing.event_bus import EventBus, event_bus


def _recorder(tmp_path: Path) -> tuple[AuditRecorder, Database]:
    db = Database(tmp_path / "wing.db")
    assert db.ready
    return AuditRecorder(db), db


def _llm_event(**overrides: Any) -> LLMCallMetricsEvent:
    payload: dict[str, Any] = {
        "session_id": "s1",
        "request_id": "r1",
        "model": "glm-5",
        "provider": "default",
        "prompt_tokens": 100,
        "completion_tokens": 50,
        "cached_tokens": 20,
        "first_chunk_rt_ms": 123.5,
        "tokens_per_sec": 42.5,
        "duration_ms": 900,
        "stop_reason": "end_turn",
    }
    payload.update(overrides)
    return LLMCallMetricsEvent.model_validate(payload)


def _tool_event(**overrides: Any) -> ToolCallResultEvent:
    payload: dict[str, Any] = {
        "session_id": "s1",
        "request_id": "r2",
        "tool_name": "Bash",
        "tool_args": {"command": "echo hi"},
        "tool_call_id": "tc1",
        "tool_result": "hi",
        "tool_success": True,
        "model": "glm-5",
        "duration_ms": 37,
    }
    payload.update(overrides)
    return ToolCallResultEvent.model_validate(payload)


# ============================================================
# 三类行
# ============================================================


class TestRows:
    def test_llm_call_row_fields(self, tmp_path: Path) -> None:
        recorder, db = _recorder(tmp_path)
        event = _llm_event()

        recorder.on_event(event)

        rows = db.query("SELECT * FROM audit_llm_call")
        assert len(rows) == 1
        row = rows[0]
        assert row["session_id"] == "s1"
        assert row["request_id"] == "r1"
        assert row["model"] == "glm-5"
        assert row["provider"] == "default"
        assert row["prompt_tokens"] == 100
        assert row["completion_tokens"] == 50
        assert row["cached_tokens"] == 20
        assert row["first_chunk_rt_ms"] == 123.5
        assert row["tokens_per_sec"] == 42.5
        assert row["duration_ms"] == 900
        assert row["stop_reason"] == "end_turn"
        # 时间戳口径：事件的创建时刻（本地 naive ISO + epoch 毫秒）
        assert row["ts"] == event.created_at.isoformat()
        assert row["ts_ms"] == int(event.created_at.timestamp() * 1000)
        db.close()

    def test_llm_call_optional_fields_stay_null(self, tmp_path: Path) -> None:
        recorder, db = _recorder(tmp_path)
        event = _llm_event(
            stop_reason=None, session_id=None, provider="", duration_ms=0
        )

        recorder.on_event(event)

        row = db.query("SELECT * FROM audit_llm_call")[0]
        assert row["stop_reason"] is None
        assert row["session_id"] is None
        assert row["provider"] == ""
        assert row["duration_ms"] == 0  # 事件契约：0 = 未测量，原样落库
        db.close()

    def test_tool_call_row_fields(self, tmp_path: Path) -> None:
        recorder, db = _recorder(tmp_path)

        recorder.on_event(_tool_event())
        recorder.on_event(
            _tool_event(tool_call_id="tc2", tool_success=False, duration_ms=5)
        )

        rows = db.query("SELECT * FROM audit_tool_call ORDER BY id")
        assert len(rows) == 2
        assert rows[0]["tool_name"] == "Bash"
        assert rows[0]["tool_call_id"] == "tc1"
        assert rows[0]["success"] == 1
        assert rows[0]["duration_ms"] == 37
        assert rows[0]["session_id"] == "s1"
        assert rows[1]["success"] == 0
        assert rows[1]["duration_ms"] == 5
        db.close()

    def test_compaction_row_fields(self, tmp_path: Path) -> None:
        recorder, db = _recorder(tmp_path)

        recorder.on_event(
            CompactDoneEvent(
                session_id="s1",
                model="glm-5",
                original_tokens=9000,
                compressed_tokens=1200,
            )
        )

        row = db.query("SELECT * FROM audit_compaction")[0]
        assert row["session_id"] == "s1"
        assert row["model"] == "glm-5"
        assert row["original_tokens"] == 9000
        assert row["compressed_tokens"] == 1200
        db.close()


# ============================================================
# 不写行的情形
# ============================================================


class TestSkippedRows:
    def test_tool_call_without_duration_is_not_recorded(self, tmp_path: Path) -> None:
        """中断合成的结果（duration_ms=None）不是一次执行——不写行。"""
        recorder, db = _recorder(tmp_path)

        recorder.on_event(_tool_event(duration_ms=None))

        assert db.query("SELECT * FROM audit_tool_call") == []
        db.close()

    def test_compaction_without_session_is_not_recorded(self, tmp_path: Path) -> None:
        recorder, db = _recorder(tmp_path)

        recorder.on_event(
            CompactDoneEvent(original_tokens=1, compressed_tokens=1, model="m")
        )

        assert db.query("SELECT * FROM audit_compaction") == []
        db.close()

    def test_other_events_are_ignored(self, tmp_path: Path) -> None:
        recorder, db = _recorder(tmp_path)

        recorder.on_event(TextEvent(session_id="s1", content="hi"))

        for table in ("audit_llm_call", "audit_tool_call", "audit_compaction"):
            assert db.query(f"SELECT * FROM {table}") == []
        db.close()


# ============================================================
# 降级 / 幂等
# ============================================================


class TestDegradation:
    def test_degraded_database_does_not_raise(self, tmp_path: Path) -> None:
        """降级后记录事件既不抛、也写不进去（execute 返回 False、查询为空）。"""
        blocked = tmp_path / "wing.db"
        blocked.mkdir()
        db = Database(blocked)
        assert not db.ready
        recorder = AuditRecorder(db)

        recorder.on_event(_llm_event())
        recorder.on_event(_tool_event())
        recorder.on_event(
            CompactDoneEvent(session_id="s1", original_tokens=1, compressed_tokens=1)
        )

        # 负断言：整条链路是 no-op，且没有替它改写出任何东西
        assert (
            db.execute(
                "INSERT INTO audit_llm_call (ts_ms, ts, model) VALUES (1, 't', 'm')"
            )
            is False
        )
        for table in ("audit_llm_call", "audit_tool_call", "audit_compaction"):
            assert db.query(f"SELECT * FROM {table}") == []
        assert blocked.is_dir()

    def test_install_is_idempotent(self) -> None:
        """重复 install() 不追加订阅者（subscribe 是 append 语义 = 审计双写）。"""
        install()
        before = event_bus.subscriber_count
        install()
        install()
        assert event_bus.subscriber_count == before


class TestEventBusIntegration:
    def test_emit_lands_in_wing_home_database(self) -> None:
        """全链路：EventBus.emit → recorder → `<WING_HOME>/core/wing.db`。

        用**独立** EventBus 实例（不依赖全进程订阅状态——别的测试会清订阅者）：
        要验的是 emit → 记录器 → 单例库这条链；recorder 不注入 database，
        才会走到 ``get_database()`` 的路径解析。
        """
        bus = EventBus()
        bus.subscribe(AuditRecorder().on_event)

        bus.emit(_llm_event(model="audited-model"))
        bus.emit(_tool_event(tool_name="Read"))

        db = get_database()
        assert db.ready
        assert db.path.name == "wing.db"
        assert db.path.parent.name == "core"
        assert [row["model"] for row in db.query("SELECT * FROM audit_llm_call")] == [
            "audited-model"
        ]
        assert [
            row["tool_name"] for row in db.query("SELECT * FROM audit_tool_call")
        ] == ["Read"]
