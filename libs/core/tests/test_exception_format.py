"""异常链格式化契约：空消息不留空尾（`ReadError: ` 的反面）。

现场（2026-09-13）：上游 FIN 结束流式调用后，日志里只有
`call generate failed (attempt 1): ReadError: , retrying...`——异常消息为空，
类型名后面什么都没有，故障无从诊断。本文件钉死"空消息也要有信息"的契约。
"""

from __future__ import annotations

import httpx

from wing.common.utils import format_exception_chain


# ============================================================
# 异常文案
# ============================================================


class TestFormatExceptionChain:
    def test_empty_message_keeps_type_name(self):
        """`ReadError: ` 这样的空尾文案是现场诊断失效的一半原因。"""
        exc = httpx.ReadError("")
        text = format_exception_chain(exc)
        assert text.startswith("ReadError")
        assert not text.endswith(": "), text
        assert text != "ReadError: "

    def test_empty_message_with_request_context(self):
        """空消息 + 请求上下文：能指出是哪个请求的读错误。"""
        request = httpx.Request("POST", "https://api.example.com/v1/chat/completions")
        exc = httpx.ReadError("", request=request)
        text = format_exception_chain(exc)
        assert "ReadError" in text
        assert "POST https://api.example.com/v1/chat/completions" in text

    def test_non_empty_message_format_unchanged(self):
        exc = httpx.ConnectError("Connection refused")
        assert format_exception_chain(exc) == "ConnectError: Connection refused"

    def test_chain_format_unchanged(self):
        try:
            try:
                raise ValueError("inner")
            except ValueError as e:
                raise RuntimeError("outer") from e
        except RuntimeError as e:
            text = format_exception_chain(e)
        assert text == "RuntimeError: outer (caused by ValueError: inner)"

    def test_empty_message_inside_chain(self):
        """链中的空消息异常同样不留空尾。"""
        outer = RuntimeError("outer")
        outer.__cause__ = httpx.ReadError("")
        text = format_exception_chain(outer)
        assert text == "RuntimeError: outer (caused by ReadError)"
