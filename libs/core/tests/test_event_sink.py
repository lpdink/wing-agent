"""AgentEventSink 分流与 request_id 定型测试。

sink 是 agent 包内事件发射的唯一出口，按 persist 标记分流：
- persist=true → 经 append_event 即时落盘进链，再广播；
- persist=false → 纯广播，不落盘、不缓冲（瞬态内容由轮提交的 Message 记录
  承载，未提交内容由 accumulator 投影按需取得）。

request_id 在落盘之前从 RequestContext 定型注入——磁盘记录与广播帧携带同一
关联值（日志是唯一事实来源，live replay 与 resume replay 不允许对同一事件
呈现不同的 request_id）。
"""

from __future__ import annotations

import json

from wing.agent.event_sink import AgentEventSink
from wing.common.tracked_list import TrackedList
from wing.event import DiffContentEvent
from wing.event_bus import event_bus
from wing.request_context import reset_request_context, set_request_context
from wing.schema import ChainNode, ToolCall
from wing.store import FileMessageLog


def _read_log_lines(tmp_dir) -> list[dict]:
    path = tmp_dir / "history.jsonl"
    if not path.exists():
        return []
    return [
        json.loads(line)
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


class TestPersistSplit:
    def setup_method(self):
        event_bus._subscribers.clear()

    def teardown_method(self):
        event_bus._subscribers.clear()

    def test_transient_broadcast_only_persisted_to_chain(self):
        """persist=false 纯广播（不落盘、不缓冲）；persist=true 即时落盘。"""
        persisted: list = []
        broadcast: list = []
        sink = AgentEventSink(session_id="s-test", append_event=persisted.append)
        event_bus.subscribe(broadcast.append)

        sink.llm_text("hello ")  # persist=false → 纯广播
        sink.llm_text("world")
        sink._emit(DiffContentEvent(path="f", new_text="x"))  # persist=true → 落盘

        # 落盘：只有 diff 事件（流式 delta 不落盘、也不进任何缓冲）
        assert len(persisted) == 1
        assert isinstance(persisted[0], DiffContentEvent)
        assert persisted[0].path == "f"

        # 广播：两个瞬态（逐包）+ 一个 diff = 3
        types = [type(e).__name__ for e in broadcast]
        assert types == ["TextEvent", "TextEvent", "DiffContentEvent"]

    def test_tool_call_event_transient(self):
        """ToolCallEvent 是瞬态（与 assistant Message 孪生，不落盘、不缓冲）。"""
        persisted: list = []
        sink = AgentEventSink(session_id="s", append_event=persisted.append)
        tc = ToolCall(id="c1", name="Bash", arguments={})
        sink.tool_started(tc)

        # 不落盘，也不留任何内存副本（瞬态内容由轮提交的 Message 记录承载）
        assert persisted == []

    def test_no_append_callback_is_pure_broadcast(self):
        """append_event=None（无持久化语义场景）：persist=true 也不落盘，只广播。"""
        broadcast: list = []
        sink = AgentEventSink(session_id="s", append_event=None)
        event_bus.subscribe(broadcast.append)
        sink._emit(DiffContentEvent(path="f", new_text="x"))
        assert len(broadcast) == 1


class TestRequestIdFinalization:
    """request_id 在落盘前定型——磁盘记录与广播帧携带同一关联值。"""

    def setup_method(self):
        event_bus._subscribers.clear()

    def teardown_method(self):
        event_bus._subscribers.clear()

    def test_persisted_record_carries_request_context_id(self, tmp_path):
        """ctx 有 request_id 时，磁盘行的 request_id == ctx 值。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        sink = AgentEventSink(session_id="s", append_event=tl.append)
        token = set_request_context(request_id="req-from-client", session_id="s")
        try:
            sink._emit(DiffContentEvent(path="f", new_text="x"))
        finally:
            reset_request_context(token)

        (record,) = _read_log_lines(tmp_path)
        assert record["type"] == "diff_content"
        assert record["request_id"] == "req-from-client"
        # 内存链上对象与磁盘一致（广播后的覆写是幂等 no-op，无漂移）
        node = tl.active_chain[0]
        assert isinstance(node, DiffContentEvent)
        assert node.request_id == "req-from-client"

    def test_no_request_context_keeps_backend_generated_id(self, tmp_path):
        """ctx 无 request_id 时保留后端生成值，内存与磁盘一致。"""
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        sink = AgentEventSink(session_id="s", append_event=tl.append)
        sink._emit(DiffContentEvent(path="f", new_text="x"))

        (record,) = _read_log_lines(tmp_path)
        assert record["request_id"]  # 非空：后端生成的 uuid4 hex
        node = tl.active_chain[0]
        assert isinstance(node, DiffContentEvent)
        assert record["request_id"] == node.request_id

    def test_broadcast_frame_and_disk_record_share_request_id(self, tmp_path):
        """广播帧与磁盘记录对同一事件呈现同一 request_id（唯一事实来源）。"""
        from wing.event import wire_dump

        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        sink = AgentEventSink(session_id="s", append_event=tl.append)
        broadcast: list = []
        event_bus.subscribe(broadcast.append)
        token = set_request_context(request_id="req-shared", session_id="s")
        try:
            sink._emit(DiffContentEvent(path="f", new_text="x"))
        finally:
            reset_request_context(token)

        (record,) = _read_log_lines(tmp_path)
        frame = wire_dump(broadcast[0])
        assert record["request_id"] == frame["request_id"] == "req-shared"
