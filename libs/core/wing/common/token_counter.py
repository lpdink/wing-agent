# wing/common/token_counter.py
"""
轻量级 token 估算，用于无服务端 usage 时的 fallback。

优先使用服务端回传的 usage.prompt_tokens（最准确），
TokenCounter 仅在以下场景使用：
  - 新 session 没有 assistant 消息（无 usage）
  - provider 不回传 usage
  - web fetch 等工具需要估算 token 量

估算原理：CJK 字符 × 0.7 + 非 CJK 字符 × 0.3，
实测比真实值多约 10%，略高估对 compact 判断安全。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from wing.schema import Message


class TokenCounter:
    """纯静态类，无依赖，无实例化。"""

    CJK_RATIO = 0.7  # token / char for CJK
    NON_CJK_RATIO = 0.3  # token / char for non-CJK

    @staticmethod
    def _is_cjk(ch: str) -> bool:
        cp = ord(ch)
        return (
            0x4E00 <= cp <= 0x9FFF  # CJK Unified Ideographs
            or 0x3400 <= cp <= 0x4DBF  # CJK Extension A
            or 0xF900 <= cp <= 0xFAFF  # CJK Compatibility Ideographs
            or 0x2E80 <= cp <= 0x2EFF  # CJK Radicals Supplement
        )

    @classmethod
    def count(cls, text: str) -> int:
        """估算文本的 token 数量。"""
        cjk = 0
        non_cjk = 0
        for ch in text:
            if cls._is_cjk(ch):
                cjk += 1
            else:
                non_cjk += 1
        return int(cjk * cls.CJK_RATIO + non_cjk * cls.NON_CJK_RATIO)

    @classmethod
    def encode(cls, text: str) -> list[int]:
        """Compatibility shim: returns dummy list whose length == count.

        替代 tiktoken.get_encoding("o200k_base").encode()，
        部分代码用 len(encode(text)) 计算 token 数。
        """
        return [1] * cls.count(text)

    @classmethod
    def estimate_message(cls, msg: Message) -> int:
        """估算单条消息的 token 数（基于 repr）。"""
        return cls.count(repr(msg))
