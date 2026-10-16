"""收口事实（``last_turn_end`` / ``last_turn_subtype``）——落盘、投影与顺序红线。

锚点（任务书 §数据模型 / §写点）：

- 写点搭 ``_on_usage_event`` 的 ``TurnResultEvent`` 分支既有 flush 便车：回合收口
  即落盘，每轮零新增写盘；
- **顺序红线**：字段先于 ``record_turn()`` 写入——节流窗口到期时 ``_touch`` 会
  立刻落盘（随后的显式 ``flush()`` 因脏标记已清而是 no-op），字段晚写就只留在
  内存里；
- 序列化排除 None：旧 metadata.json（无此字段）零变化、读出 None，永不伪装成
  已完成；
- 投影：``build_session_info`` 带上两字段；resume 后从盘上延续。
"""

from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path

import pytest

from wing.event_bus import event_bus
from wing.gateway.projection import build_session_info
from wing.store import SessionMetadata
from wing.store.file import FileSessionStore


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    """每个测试前后清理全局 EventBus（Session 构造即订阅）。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def runtime():
    from wing.runtime import WingRuntime

    return WingRuntime()


def _metadata_of(runtime, session_id: str) -> SessionMetadata | None:
    return runtime.sm._stores["file"].load_metadata(session_id)


class _Clock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now


# ============================================================
# 模型与序列化边界
# ============================================================


def test_metadata_round_trip_and_none_exclusion(tmp_path: Path):
    """有值落盘可读回；None 不进文件（旧 metadata 零字段变化）。"""
    store = FileSessionStore(tmp_path / "sessions")
    store.save_metadata(
        "sess-values",
        SessionMetadata(
            session_name="done",
            last_turn_end="2026-01-01T10:00:00.123456",
            last_turn_subtype="error_max_turns",
        ),
    )
    raw = json.loads(
        (store._root / "sess-values" / "metadata.json").read_text(encoding="utf-8")
    )
    assert raw["last_turn_end"] == "2026-01-01T10:00:00.123456"
    assert raw["last_turn_subtype"] == "error_max_turns"
    loaded = store.load_metadata("sess-values")
    assert loaded is not None
    assert loaded.last_turn_end == "2026-01-01T10:00:00.123456"
    assert loaded.last_turn_subtype == "error_max_turns"

    store.save_metadata("sess-none", SessionMetadata(session_name="fresh"))
    raw_none = json.loads(
        (store._root / "sess-none" / "metadata.json").read_text(encoding="utf-8")
    )
    assert "last_turn_end" not in raw_none
    assert "last_turn_subtype" not in raw_none


def test_legacy_metadata_without_fields_reads_none(tmp_path: Path):
    """旧文件（引入前的形态）读出的就是 None（不发明数字、不冒充已完成）。"""
    store = FileSessionStore(tmp_path / "sessions")
    session_dir = store._root / "sess-legacy"
    session_dir.mkdir(parents=True)
    (session_dir / "metadata.json").write_text(
        json.dumps({"session_name": "old", "last_interaction": "2025-01-01T00:00:00"}),
        encoding="utf-8",
    )

    metadata = store.load_metadata("sess-legacy")
    assert metadata is not None
    assert metadata.last_turn_end is None
    assert metadata.last_turn_subtype is None


# ============================================================
# 写点：回合收口落盘 + 投影
# ============================================================


@pytest.mark.asyncio
async def test_turn_result_persists_fields_and_projects_them(runtime):
    session = runtime.create_session()
    sid = session.session_id

    # 未收口：两字段都无（info 投影与盘上一致，都是 None）。
    assert session.last_turn_end is None
    assert session.last_turn_subtype is None
    info = build_session_info(session)
    assert info.last_turn_end is None and info.last_turn_subtype is None

    session.agent.sink.turn_result(subtype="success", num_turns=1, duration_ms=1500)

    assert session.last_turn_subtype == "success"
    stamped = session.last_turn_end
    assert stamped is not None
    datetime.fromisoformat(stamped)  # 本地 naive ISO（与 last_interaction 同口径）

    recorded = _metadata_of(runtime, sid)
    assert recorded is not None
    assert recorded.last_turn_end == stamped
    assert recorded.last_turn_subtype == "success"
    info = build_session_info(session)
    assert info.last_turn_end == stamped
    assert info.last_turn_subtype == "success"


@pytest.mark.asyncio
async def test_throttled_flush_inside_record_turn_still_persists_fields(runtime):
    """节流窗口到期时 ``_touch`` 在 ``record_turn`` 内即刻落盘——字段必须已经在。

    这是写点顺序的钉子：字段若在 ``record_turn()`` 之后写，这一次落盘带的就是
    None，而随后的显式 ``flush()`` 因脏标记已清成为 no-op——字段只留在内存里。
    """
    session = runtime.create_session()
    sid = session.session_id
    tracker = session._usage  # noqa: SLF001

    clock = _Clock()
    clock.now = 10.0  # 距上次落盘 ≥ FLUSH_INTERVAL_S：``_touch`` 记完即落
    tracker._clock = clock  # noqa: SLF001
    tracker._last_flush = 0.0  # noqa: SLF001

    written: list[str | None] = []
    original_save = session.store.save_metadata

    def _save(session_id: str, metadata: SessionMetadata) -> None:
        written.append(metadata.last_turn_end)
        original_save(session_id, metadata)

    dirty_at_flush: list[bool] = []
    original_flush = tracker.flush

    def _flush() -> None:
        dirty_at_flush.append(tracker._dirty)  # noqa: SLF001
        original_flush()

    session.store.save_metadata = _save  # type: ignore[method-assign]
    tracker.flush = _flush  # type: ignore[method-assign]

    session.agent.sink.turn_result(subtype="success", num_turns=1, duration_ms=1)

    # ① 收口路径上发生了两次 flush：节流内的那次（脏）与收尾的显式那次（已无脏
    #    ——落盘已经发生过），说明节流那条路径真的走到了。
    assert dirty_at_flush == [True, False], dirty_at_flush
    # ② 而它写出的唯一一份 metadata 已经带着字段（中途的 ``_dirty=True`` 快照
    #    由节流那次 flush 落盘）。
    assert written == [session.last_turn_end], written
    assert session.last_turn_subtype == "success"
    recorded = _metadata_of(runtime, sid)
    assert recorded is not None
    assert recorded.last_turn_end == session.last_turn_end


@pytest.mark.asyncio
async def test_subtype_is_passed_through_verbatim(runtime):
    """错误收口同样是一次收口（模型不会继续了）：subtype 逐字透传，不改写。"""
    session = runtime.create_session()
    session.agent.sink.turn_result(
        subtype="error_max_turns", is_error=True, num_turns=3, duration_ms=9
    )

    assert session.last_turn_subtype == "error_max_turns"
    recorded = _metadata_of(runtime, session.session_id)
    assert recorded is not None
    assert recorded.last_turn_subtype == "error_max_turns"


@pytest.mark.asyncio
async def test_next_turn_overwrites_and_fields_survive_resume(runtime):
    """下一次收口覆盖；逐出 + resume 后从盘上延续（跨进程事实）。"""
    session = runtime.create_session()
    sid = session.session_id
    session.agent.sink.turn_result(subtype="success", num_turns=1, duration_ms=1)
    first_end = session.last_turn_end
    assert first_end is not None

    assert runtime.sm.evict(sid, reason="test")
    await runtime.sm.wait_teardowns()

    resumed = runtime.sm.resume_session(sid)
    assert resumed.last_turn_end == first_end
    assert resumed.last_turn_subtype == "success"

    resumed.agent.sink.turn_result(
        subtype="error_during_execution", is_error=True, num_turns=2, duration_ms=4
    )
    assert resumed.last_turn_subtype == "error_during_execution"
    second_end = resumed.last_turn_end
    assert second_end is not None and second_end >= first_end
    recorded = _metadata_of(runtime, sid)
    assert recorded is not None
    assert recorded.last_turn_end == second_end
    assert recorded.last_turn_subtype == "error_during_execution"
