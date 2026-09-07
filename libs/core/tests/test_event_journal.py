"""EventJournal 合成语义与收口清空测试。"""

from __future__ import annotations

from wing.agent.event_journal import EventJournal
from wing.event import (
    DiffContentEvent,
    ReasoningEvent,
    TextEvent,
    ToolCallEvent,
    ToolCallStreamEvent,
    TurnStartedEvent,
)
from wing.schema import ToolCall


class TestDeltaMerge:
    def test_consecutive_text_merged_into_one(self):
        """连续 100 条 text delta → journal 持有 1 个合成包。"""
        j = EventJournal()
        for i in range(100):
            j.record(TextEvent(content=f"chunk{i}"))
        snapshot = j.snapshot()
        assert len(snapshot) == 1
        assert isinstance(snapshot[0], TextEvent)
        assert snapshot[0].content == "".join(f"chunk{i}" for i in range(100))

    def test_interleaved_preserves_order(self):
        """交错 thinking/text 不被重排：按相邻同类分多包。"""
        j = EventJournal()
        j.record(ReasoningEvent(content="think-1 "))
        j.record(ReasoningEvent(content="more"))
        j.record(TextEvent(content="say-1 "))
        j.record(TextEvent(content="more"))
        j.record(ReasoningEvent(content="think-2"))

        snapshot = j.snapshot()
        assert len(snapshot) == 3
        assert isinstance(snapshot[0], ReasoningEvent)
        assert snapshot[0].content == "think-1 more"
        assert isinstance(snapshot[1], TextEvent)
        assert snapshot[1].content == "say-1 more"
        assert isinstance(snapshot[2], ReasoningEvent)
        assert snapshot[2].content == "think-2"

    def test_tool_call_stream_merged_by_call_id(self):
        """同一 tool_call_id 的参数流按 id 合并（可与其他事件交错）。"""
        j = EventJournal()
        j.record(
            ToolCallStreamEvent(
                tool_call_id="a", tool_name="Bash", args_fragment='{"comm'
            )
        )
        j.record(TextEvent(content="interleaved text"))
        j.record(
            ToolCallStreamEvent(
                tool_call_id="a", tool_name="Bash", args_fragment='and": "ls"}'
            )
        )
        j.record(
            ToolCallStreamEvent(
                tool_call_id="b", tool_name="Read", args_fragment='{"path"'
            )
        )

        snapshot = j.snapshot()
        # 3 个包：a 的合成包、text、b 的首包（顺序 = 首次出现序）
        assert len(snapshot) == 3
        a = snapshot[0]
        assert isinstance(a, ToolCallStreamEvent)
        assert a.tool_call_id == "a"
        assert a.args_fragment == '{"command": "ls"}'
        assert a.is_final is False
        assert isinstance(snapshot[1], TextEvent)
        b = snapshot[2]
        assert isinstance(b, ToolCallStreamEvent) and b.tool_call_id == "b"

    def test_tool_call_stream_is_final_semantics(self):
        """is_final 保留：后续无 fragment 的终结标记不覆盖已拼接内容。"""
        j = EventJournal()
        j.record(
            ToolCallStreamEvent(
                tool_call_id="a", tool_name="Bash", args_fragment='{"x"'
            )
        )
        j.record(
            ToolCallStreamEvent(
                tool_call_id="a", tool_name="Bash", args_fragment=": 1}"
            )
        )
        j.record(
            ToolCallStreamEvent(
                tool_call_id="a", tool_name="Bash", args_fragment="", is_final=True
            )
        )

        snapshot = j.snapshot()
        assert len(snapshot) == 1
        a = snapshot[0]
        assert isinstance(a, ToolCallStreamEvent)
        assert a.args_fragment == '{"x": 1}'
        assert a.is_final is True


class TestNonDeltaPassthrough:
    def test_non_delta_events_buffered_in_order(self):
        """非 delta 瞬态事件原样缓冲、保序、不合成。"""
        j = EventJournal()
        j.record(TurnStartedEvent())
        j.record(
            ToolCallEvent(
                tool_name="Bash",
                tool_args={"command": "ls"},
                tool_call_id="c1",
            )
        )
        snapshot = j.snapshot()
        assert len(snapshot) == 2
        assert isinstance(snapshot[0], TurnStartedEvent)
        assert isinstance(snapshot[1], ToolCallEvent)
        assert snapshot[1].tool_call_id == "c1"


class TestClear:
    def test_clear_empties_journal(self):
        j = EventJournal()
        j.record(TextEvent(content="a"))
        j.record(
            ToolCallStreamEvent(tool_call_id="x", tool_name="T", args_fragment="1")
        )
        assert len(j) == 2

        j.clear()
        assert len(j) == 0
        assert j.snapshot() == []

        # 清空后同一 tool_call_id 的后续 delta 是新包（新一轮流）
        j.record(
            ToolCallStreamEvent(tool_call_id="x", tool_name="T", args_fragment="2")
        )
        assert len(j) == 1
        first = j.snapshot()[0]
        assert isinstance(first, ToolCallStreamEvent)
        assert first.args_fragment == "2"

    def test_snapshot_is_copy(self):
        """snapshot 返回列表浅拷贝——外部持有不影响后续记录。"""
        j = EventJournal()
        j.record(TextEvent(content="a"))
        snap = j.snapshot()
        j.record(TextEvent(content="b"))
        assert len(snap) == 1
        assert len(j.snapshot()) == 1  # b 与 a 相邻同类合并
        merged = j.snapshot()[0]
        assert isinstance(merged, TextEvent)
        assert merged.content == "ab"


class TestPersistSplit:
    """persist 分流：sink 层（journal vs 落盘）的行为验证。"""

    def test_sink_routes_transient_to_journal_and_persisted_to_chain(self):
        from wing.agent.event_sink import AgentEventSink
        from wing.event_bus import event_bus

        persisted: list = []
        broadcast: list = []
        sink = AgentEventSink(session_id="s-test", append_event=persisted.append)
        event_bus._subscribers.clear()
        event_bus.subscribe(broadcast.append)
        try:
            sink.llm_text("hello ")  # persist=false → journal
            sink.llm_text("world")
            sink._emit(DiffContentEvent(path="f", new_text="x"))  # persist=true → 落盘

            # journal：一条合成 text 包
            snapshot = sink.journal.snapshot()
            assert len(snapshot) == 1
            merged = snapshot[0]
            assert isinstance(merged, TextEvent)
            assert merged.content == "hello world"

            # 落盘：只有 diff 事件
            assert len(persisted) == 1
            assert isinstance(persisted[0], DiffContentEvent)
            assert persisted[0].path == "f"

            # 广播：两个瞬态（逐包）+ 一个 diff = 3
            types = [type(e).__name__ for e in broadcast]
            assert types == ["TextEvent", "TextEvent", "DiffContentEvent"]

            sink.clear_journal()
            assert len(sink.journal) == 0
        finally:
            event_bus._subscribers.clear()

    def test_tool_call_event_transient(self):
        """ToolCallEvent 是瞬态（与 assistant Message 孪生，不落盘）。"""
        from wing.agent.event_sink import AgentEventSink

        persisted: list = []
        sink = AgentEventSink(session_id="s", append_event=persisted.append)
        tc = ToolCall(id="c1", name="Bash", arguments={})
        sink.tool_started(tc)

        assert len(persisted) == 0
        assert len(sink.journal) == 1
        assert isinstance(sink.journal.snapshot()[0], ToolCallEvent)


class TestRequestIdFinalization:
    """request_id 在落盘前定型——磁盘记录与广播帧携带同一关联值。

    日志是唯一事实来源：live replay 与 resume replay 对同一事件不允许
    呈现不同的 request_id（否则按 request_id 串联日志时断裂）。
    """

    @staticmethod
    def _read_log_lines(tmp_dir) -> list[dict]:
        import json

        path = tmp_dir / "history.jsonl"
        return [
            json.loads(line)
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]

    def test_persisted_record_carries_request_context_id(self, tmp_path):
        """ctx 有 request_id 时，磁盘行的 request_id == ctx 值。"""
        from wing.agent.event_sink import AgentEventSink
        from wing.common.tracked_list import TrackedList
        from wing.event_bus import event_bus
        from wing.request_context import (
            reset_request_context,
            set_request_context,
        )
        from wing.schema import ChainNode
        from wing.store import FileMessageLog

        event_bus._subscribers.clear()
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        sink = AgentEventSink(session_id="s", append_event=tl.append)
        token = set_request_context(request_id="req-from-client", session_id="s")
        try:
            sink._emit(DiffContentEvent(path="f", new_text="x"))
        finally:
            reset_request_context(token)
            event_bus._subscribers.clear()

        # 磁盘行（jsonl 序列化快照）携带 ctx 的 request_id
        (record,) = self._read_log_lines(tmp_path)
        assert record["type"] == "diff_content"
        assert record["request_id"] == "req-from-client"
        # 内存链上对象与磁盘一致（广播后的覆写是幂等 no-op，无漂移）
        node = tl.active_chain[0]
        assert isinstance(node, DiffContentEvent)
        assert node.request_id == "req-from-client"

    def test_no_request_context_keeps_backend_generated_id(self, tmp_path):
        """ctx 无 request_id 时保留后端生成值，内存与磁盘一致。"""
        from wing.agent.event_sink import AgentEventSink
        from wing.common.tracked_list import TrackedList
        from wing.event_bus import event_bus
        from wing.schema import ChainNode
        from wing.store import FileMessageLog

        event_bus._subscribers.clear()
        tl: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_path))
        sink = AgentEventSink(session_id="s", append_event=tl.append)
        sink._emit(DiffContentEvent(path="f", new_text="x"))
        event_bus._subscribers.clear()

        (record,) = self._read_log_lines(tmp_path)
        assert record["request_id"]  # 非空：后端生成的 uuid4 hex
        node = tl.active_chain[0]
        assert isinstance(node, DiffContentEvent)
        assert record["request_id"] == node.request_id
