"""序列化边界测试：persist ClassVar、剥 null、wire 帧剥离、存量孪生容忍。

三个序列化边界，三套剥离规则（不可用一个 model_dump(exclude_none) 打通——
Message._serialize_flat 是 wrap 序列化器，content/reasoning_content/tool_calls
在内层 handler 之后才注入）：

  磁盘记录   TrackedList._to_record   剥 null + 剥 target + disk_exclude
  WS 直播帧  wire_dump                剥 null + 剥 parent_uuid/unzip_last_uuid/role/target，保留 uuid
  SyncSession serialize_event         同 wire_dump（统一出口）
"""

from __future__ import annotations

import json
from pathlib import Path

from wing.common.tracked_list import TrackedList
from wing.context_manager import ContextManager
from wing.event import (
    FACT_EVENTS,
    DiffContentEvent,
    LLMCallMetricsEvent,
    TextEvent,
    ToolCallResultEvent,
    serialize_event,
    wire_dump,
)
from wing.schema import ChainNode, Message, TextBlock
from wing.store import FileMessageLog


def _read_history(path: Path) -> list[dict]:
    hist = path / "history.jsonl"
    if not hist.exists():
        return []
    return [
        json.loads(line)
        for line in hist.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _make_cm(messages: TrackedList[ChainNode], sid: str = "s") -> ContextManager:
    from wing.compactor import Compactor

    return ContextManager(
        session_id=sid,
        messages=messages,
        system_prompt="x",
        compactor=Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000),
    )


# ============================================================
# 6.5 persist 为 ClassVar——不被子类重声明击穿
# ============================================================


class TestPersistClassVar:
    def test_redeclared_and_inherited_both_absent_from_serialization(self):
        """重声明 persist=False 与未重声明（继承 True）的事件序列化都不含 persist 键。

        旧缺陷：`Field(exclude=True)` 被子类重声明静默击穿——重声明的
        TextEvent 线上带 "persist":false，未重声明的 DiffContentEvent 反而不带，
        与"从序列化中排除"的意图完全相反。ClassVar 不是 pydantic 字段，无此陷阱。
        """
        text = TextEvent(content="x")  # 重声明 persist=False
        diff = DiffContentEvent(path="f", new_text="y")  # 未重声明，继承 True

        # 属性读取仍分别为 False / True
        assert text.persist is False
        assert diff.persist is True

        # model_dump 与 wire 帧都不含 persist 键
        assert "persist" not in text.model_dump()
        assert "persist" not in diff.model_dump()
        assert "persist" not in wire_dump(text)
        assert "persist" not in wire_dump(diff)

    def test_persist_absent_from_disk_record(self, tmp_path):
        """落盘记录不含 persist 键（ClassVar 非字段）。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        tl.append(DiffContentEvent(path="f", new_text="y"))
        (record,) = _read_history(tmp_path)
        assert "persist" not in record
        assert record["type"] == "diff_content"


# ============================================================
# 6.6 _to_record 剥 null 的往返等价
# ============================================================


class TestToRecordStripNull:
    def test_wrap_derived_null_fields_stripped_and_roundtrip(self, tmp_path):
        """wrap 序列化器派生的 reasoning_content/tool_calls（值 None）被剥除，往返等价。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        # assistant 只有 text 块 → reasoning_content / tool_calls 派生为 None
        msg = Message(role="assistant", content_blocks=[TextBlock(text="hi")])
        tl.append(msg)

        (record,) = _read_history(tmp_path)
        # 派生 null 字段被剥除（exclude_none 看不到它们——必须字典过滤）
        assert "reasoning_content" not in record
        assert "tool_calls" not in record
        assert "unzip_last_uuid" not in record
        assert "tool_call_id" not in record
        # 非 null 保留
        assert record["content"] == "hi"
        assert record["content_blocks"] == [{"type": "text", "text": "hi"}]
        assert record["role"] == "assistant"
        assert record["uuid"] == msg.uuid

        # 往返：还原对象逐字段相等
        tl2 = TrackedList.load(FileMessageLog(tmp_path), Message)
        restored = tl2.active_chain[0]
        assert isinstance(restored, Message)
        assert restored.content == "hi"
        assert restored.reasoning_content is None
        assert restored.tool_calls is None
        assert restored.uuid == msg.uuid

    def test_diff_old_text_none_new_file_semantics_preserved(self, tmp_path):
        """DiffContentEvent.old_text=None（新建文件）剥除后往返语义不变。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        tl.append(DiffContentEvent(path="f", old_text=None, new_text="x"))

        (record,) = _read_history(tmp_path)
        assert "old_text" not in record  # null 剥除
        assert record["new_text"] == "x"

        tl2 = TrackedList.load(FileMessageLog(tmp_path), Message)
        restored = tl2.active_chain[0]
        assert isinstance(restored, DiffContentEvent)
        assert restored.old_text is None  # "新建文件（全绿）"语义不变
        assert restored.new_text == "x"


# ============================================================
# 6.7 wire 帧剥除存储专用字段
# ============================================================


class TestWireFrame:
    def test_streaming_delta_frame_strips_storage_fields(self):
        """TextEvent 帧不含 parent_uuid/unzip_last_uuid/role/persist/target；null 一并剥除。"""
        ev = TextEvent(content="x")  # session_id None
        frame = wire_dump(ev)
        for k in ("parent_uuid", "unzip_last_uuid", "role", "persist", "target"):
            assert k not in frame
        # null session_id 被剥除
        assert "session_id" not in frame
        # 事实字段保留
        assert frame["type"] == "text"
        assert frame["content"] == "x"
        assert frame["request_id"]

    def test_session_id_kept_when_present(self):
        ev = TextEvent(content="x", session_id="s-1")
        assert wire_dump(ev)["session_id"] == "s-1"

    def test_serialize_event_matches_wire_dump(self):
        """SyncSession 载荷与直播帧同规则（serialize_event 是 wire_dump 的别名）。"""
        ev = DiffContentEvent(path="f", old_text=None, new_text="x")
        assert serialize_event(ev) == wire_dump(ev)

    def test_uuid_preserved_for_turn_level_events(self):
        """uuid 保留（AssistantTurn/TurnResult 的 uuid 被 stdio 前端消费）。"""
        from wing.event import TurnResultEvent

        frame = wire_dump(TurnResultEvent(result="r", num_turns=1))
        assert frame["uuid"]  # 非空且保留


# ============================================================
# 6.8 存量孪生记录：加载进链但不下发
# ============================================================


class TestLegacyTwinTolerance:
    def _write_legacy_log(self, path: Path) -> None:
        """写一份本变更之前的 history.jsonl（含孪生事件记录）。"""
        path.mkdir(parents=True, exist_ok=True)
        records = [
            {"role": "user", "content": "hi", "uuid": "u1"},
            {
                "role": "assistant",
                "content_blocks": [{"type": "text", "text": "hello"}],
                "uuid": "a1",
                "parent_uuid": "u1",
            },
            {
                "role": "event",
                "type": "tool_call_result",
                "tool_name": "Bash",
                "tool_args": {},
                "tool_call_id": "tc1",
                "tool_result": "ok",
                "tool_success": True,
                "uuid": "e1",
                "parent_uuid": "a1",
            },
            {
                "role": "event",
                "type": "llm_call_metrics",
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "cached_tokens": 0,
                "first_chunk_rt_ms": 1.0,
                "tokens_per_sec": 1.0,
                "uuid": "e2",
                "parent_uuid": "e1",
            },
            {
                "role": "event",
                "type": "diff_content",
                "path": "f",
                "new_text": "x",
                "tool_call_id": "tc1",
                "uuid": "e3",
                "parent_uuid": "e2",
            },
        ]
        (path / "history.jsonl").write_text(
            "\n".join(json.dumps(r, ensure_ascii=False) for r in records) + "\n",
            encoding="utf-8",
        )

    def test_legacy_twins_load_into_chain_but_not_dispatched(self, tmp_path):
        """存量孪生记录正常加载进链（前向容忍），但 get_active_events 不下发它们。"""
        self._write_legacy_log(tmp_path)
        loaded = TrackedList.load(FileMessageLog(tmp_path), Message)

        # 加载进链：孪生事件被 EVENT_TYPES 认得，正常还原（零迁移）
        kinds = {type(x).__name__ for x in loaded.active_chain}
        assert "ToolCallResultEvent" in kinds
        assert "LLMCallMetricsEvent" in kinds
        assert "DiffContentEvent" in kinds

        # 不下发：get_active_events 按 FACT_EVENTS 过滤，孪生不在集合中
        cm = _make_cm(loaded)
        dispatched = cm.get_active_events()
        dispatched_types = {e.type for e in dispatched}
        assert dispatched_types == {"diff_content"}
        assert not any(isinstance(e, ToolCallResultEvent) for e in dispatched)
        assert not any(isinstance(e, LLMCallMetricsEvent) for e in dispatched)

    def test_fact_events_excludes_twins(self):
        """孪生类型不在 FACT_EVENTS（下发过滤的单点策略）。"""
        assert "tool_call_result" not in FACT_EVENTS
        assert "llm_call_metrics" not in FACT_EVENTS
        assert "diff_content" in FACT_EVENTS


# ============================================================
# 6.10 退避重试窗口内 accumulator 残留（已知限制，显式接受）
# ============================================================


class TestBackoffWindowResidueKnownLimitation:
    def test_projection_reflects_stale_residue_until_reset(self):
        """已知限制：退避窗口内未提交投影显示上次失败尝试的残留内容。

        provider 在每次尝试开始时重置 accumulator.state（`accumulator.state =
        新 state`），但在"上次尝试失败 → 退避 sleep → 下次尝试开始"窗口内，
        state 是上次尝试的残留。投影只读 acc.state、无 staleness 检测——这是
        被显式接受的取舍（残留内容确实生成过且已直播给在线客户端，一致性优先
        于纯净；退避窗口通常短暂）。

        本测试钉住当前行为防意外回归：若将来在 provider 退避点介入清空 state
        （跨越 with_retry 封装边界），此行为会变，测试失败提醒复核该已知限制
        是否已解除。
        """
        from wing.provider.openai_compat import OpenAICompatProvider, _OAIStreamState

        provider = OpenAICompatProvider.__new__(OpenAICompatProvider)
        acc = provider.create_accumulator()

        # 上次尝试失败后的残留 state
        residue = _OAIStreamState()
        residue.content_chunks.append("stale partial from failed attempt")
        acc.state = residue
        blocks = provider.snapshot_blocks(acc)
        assert blocks is not None
        assert blocks[0].text == "stale partial from failed attempt"  # 残留可见

        # 下次尝试开始：provider 重置 state → 投影反映新尝试
        fresh = _OAIStreamState()
        fresh.content_chunks.append("new attempt")
        acc.state = fresh
        blocks2 = provider.snapshot_blocks(acc)
        assert blocks2[0].text == "new attempt"
