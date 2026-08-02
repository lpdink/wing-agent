# tests/test_sse_parser.py
"""SSE 解析器单元测试。"""

import pytest

from wing.provider.sse import SSEEvent, SSEParser, parse_json_event, parse_sse_stream


class TestSSEParser:
    def test_standard_data_event(self):
        parser = SSEParser()
        assert parser.feed('data: {"a":1}') is None
        event = parser.feed("")
        assert event is not None
        assert event.data == '{"a":1}'
        assert event.event == ""

    def test_multiple_events(self):
        parser = SSEParser()
        events = []
        for line in ['data: {"a":1}', "", 'data: {"b":2}', ""]:
            e = parser.feed(line)
            if e:
                events.append(e)
        assert len(events) == 2
        assert events[0].data == '{"a":1}'
        assert events[1].data == '{"b":2}'

    def test_comment_line_ignored(self):
        parser = SSEParser()
        assert parser.feed(": ping") is None
        assert parser.feed(": keep-alive") is None
        # 注释后接正常事件
        parser.feed('data: {"x":1}')
        event = parser.feed("")
        assert event is not None
        assert event.data == '{"x":1}'

    def test_multiline_data(self):
        parser = SSEParser()
        parser.feed("data: line1")
        parser.feed("data: line2")
        event = parser.feed("")
        assert event is not None
        assert event.data == "line1\nline2"

    def test_event_type_field(self):
        parser = SSEParser()
        parser.feed("event: content_block_delta")
        parser.feed('data: {"delta":"hi"}')
        event = parser.feed("")
        assert event is not None
        assert event.event == "content_block_delta"
        assert event.data == '{"delta":"hi"}'

    def test_empty_lines_without_data_no_event(self):
        parser = SSEParser()
        assert parser.feed("") is None
        assert parser.feed("") is None

    def test_field_without_value(self):
        parser = SSEParser()
        parser.feed("data")
        event = parser.feed("")
        assert event is not None
        assert event.data == ""

    def test_data_with_colon_in_value(self):
        parser = SSEParser()
        parser.feed('data: {"url":"http://example.com"}')
        event = parser.feed("")
        assert event is not None
        assert event.data == '{"url":"http://example.com"}'

    def test_event_type_resets_after_emit(self):
        parser = SSEParser()
        parser.feed("event: foo")
        parser.feed("data: x")
        e1 = parser.feed("")
        assert e1 is not None and e1.event == "foo"
        # 下一个事件没有 event 字段
        parser.feed("data: y")
        e2 = parser.feed("")
        assert e2 is not None and e2.event == ""


class TestParseSSEStream:
    @staticmethod
    async def _lines_from(lines: list[str]):
        for line in lines:
            yield line

    @pytest.mark.asyncio
    async def test_standard_stream(self):
        lines = ['data: {"a":1}', "", 'data: {"b":2}', "", "data: [DONE]", ""]
        events = []
        async for event in parse_sse_stream(self._lines_from(lines)):
            events.append(event)
        assert len(events) == 2
        assert events[0].data == '{"a":1}'
        assert events[1].data == '{"b":2}'

    @pytest.mark.asyncio
    async def test_done_terminates(self):
        lines = ["data: [DONE]", "", 'data: {"after":true}', ""]
        events = []
        async for event in parse_sse_stream(self._lines_from(lines)):
            events.append(event)
        assert len(events) == 0

    @pytest.mark.asyncio
    async def test_keepalive_skipped(self):
        lines = [": ping", "", 'data: {"x":1}', ""]
        events = []
        async for event in parse_sse_stream(self._lines_from(lines)):
            events.append(event)
        assert len(events) == 1
        assert events[0].data == '{"x":1}'

    @pytest.mark.asyncio
    async def test_anthropic_style_events(self):
        lines = [
            "event: content_block_start",
            'data: {"type":"content_block_start","index":0}',
            "",
            "event: content_block_delta",
            'data: {"type":"content_block_delta","delta":{"text":"hi"}}',
            "",
            "event: message_stop",
            'data: {"type":"message_stop"}',
            "",
        ]
        events = []
        async for event in parse_sse_stream(self._lines_from(lines)):
            events.append(event)
        assert len(events) == 3
        assert events[0].event == "content_block_start"
        assert events[1].event == "content_block_delta"
        assert events[2].event == "message_stop"


class TestParseJsonEvent:
    def test_valid_json(self):
        event = SSEEvent(data='{"key": "value"}')
        result = parse_json_event(event)
        assert result == {"key": "value"}

    def test_invalid_json(self):
        event = SSEEvent(data="not json")
        assert parse_json_event(event) is None

    def test_empty_data(self):
        event = SSEEvent(data="")
        assert parse_json_event(event) is None
