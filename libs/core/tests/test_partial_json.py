# tests/test_partial_json.py — parse_streaming_json 单元测试

import pytest

from wing.common.partial_json import parse_streaming_json


class TestParseStreamingJson:
    """parse_streaming_json 容错解析测试。"""

    # ── Level 1: 完整 JSON 直接解析 ──

    def test_complete_json(self):
        assert parse_streaming_json('{"command": "ls -la"}') == {"command": "ls -la"}

    def test_complete_nested(self):
        raw = '{"path": "/tmp/x.py", "content": "print(1)"}'
        result = parse_streaming_json(raw)
        assert result == {"path": "/tmp/x.py", "content": "print(1)"}

    def test_empty_string(self):
        assert parse_streaming_json("") == {}

    def test_whitespace_only(self):
        assert parse_streaming_json("   ") == {}

    def test_non_dict_json(self):
        """数组等非 dict 类型返回 {}。"""
        assert parse_streaming_json("[1, 2, 3]") == {}

    # ── Level 2: 不完整 JSON 补全解析 ──

    def test_unterminated_string_value(self):
        raw = '{"command": "ls -la'
        result = parse_streaming_json(raw)
        assert result.get("command") == "ls -la"

    def test_unterminated_key(self):
        raw = '{"comm'
        result = parse_streaming_json(raw)
        # Rust parser: key with no value → null; Python fallback: empty string.
        assert "comm" in result
        assert result.get("comm") in ("", None)

    def test_missing_closing_brace(self):
        raw = '{"path": "/tmp/test.py"}'
        # Already valid, but test without closing brace
        raw2 = '{"path": "/tmp/test.py"'
        result = parse_streaming_json(raw2)
        assert result.get("path") == "/tmp/test.py"

    def test_multiple_fields_partial(self):
        raw = '{"path": "/tmp/x.py", "content": "line1\\nline2'
        result = parse_streaming_json(raw)
        assert result.get("path") == "/tmp/x.py"
        assert "line1" in result.get("content", "")

    def test_trailing_comma(self):
        raw = '{"a": "1",'
        result = parse_streaming_json(raw)
        assert result.get("a") == "1"

    def test_nested_object_incomplete(self):
        raw = '{"outer": {"inner": "val'
        result = parse_streaming_json(raw)
        assert result.get("outer", {}).get("inner") == "val"

    def test_array_value_incomplete(self):
        raw = '{"items": ["a", "b'
        result = parse_streaming_json(raw)
        assert "a" in result.get("items", [])

    # ── Repair: 控制字符和非法转义 ──

    def test_raw_newline_in_string(self):
        """字符串内的原始换行符应被转义。"""
        raw = '{"content": "line1\nline2"}'
        result = parse_streaming_json(raw)
        assert result.get("content") == "line1\nline2"

    def test_raw_tab_in_string(self):
        raw = '{"content": "col1\tcol2"}'
        result = parse_streaming_json(raw)
        assert result.get("content") == "col1\tcol2"

    def test_invalid_escape_sequence(self):
        """非法转义序列（如 \\d）不应导致崩溃。"""
        raw = '{"path": "C:\\docs\\file.txt"}'
        result = parse_streaming_json(raw)
        # Should not crash; result may vary but must be a dict
        assert isinstance(result, dict)

    def test_trailing_backslash(self):
        """字符串末尾的孤立反斜杠。"""
        raw = '{"cmd": "echo hello\\'
        result = parse_streaming_json(raw)
        assert isinstance(result, dict)

    # ── Level 3: 完全无法解析 ──

    def test_garbage_input(self):
        assert parse_streaming_json("not json at all") == {}

    def test_just_opening_brace(self):
        result = parse_streaming_json("{")
        assert isinstance(result, dict)

    # ── 实际 LLM 流式场景模拟 ──

    def test_bash_command_streaming(self):
        """模拟 Bash 命令逐字到达。"""
        chunks = [
            '{"com',
            '{"command": "git',
            '{"command": "git status',
            '{"command": "git status"}',
        ]
        for chunk in chunks:
            result = parse_streaming_json(chunk)
            assert isinstance(result, dict)
        # Final chunk should be complete
        assert parse_streaming_json(chunks[-1]) == {"command": "git status"}

    def test_write_content_streaming(self):
        """模拟 Write 工具内容逐行到达。"""
        raw = '{"path": "/tmp/hello.py", "content": "def main():\\n    print'
        result = parse_streaming_json(raw)
        assert result.get("path") == "/tmp/hello.py"
        assert "def main():" in result.get("content", "")

    def test_unicode_content(self):
        raw = '{"content": "你好世界'
        result = parse_streaming_json(raw)
        assert result.get("content") == "你好世界"

    def test_incomplete_unicode_escape(self):
        """不完整的 \\u 转义（流式截断）。"""
        raw = '{"text": "hello\\u00'
        result = parse_streaming_json(raw)
        assert isinstance(result, dict)

    def test_trailing_colon(self):
        """`{"a":` — colon 后无值，应补 null 而非返回空。"""
        raw = '{"a":'
        result = parse_streaming_json(raw)
        assert result.get("a") is None  # null → Python None
        assert "a" in result

    def test_trailing_colon_with_space(self):
        raw = '{"key": '
        result = parse_streaming_json(raw)
        assert "key" in result
