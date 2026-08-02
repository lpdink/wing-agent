# wing/provider/http.py
"""provider 层共享的 httpx 构造（两个协议同构）。"""

from __future__ import annotations

import httpx

# 对齐旧 OpenAI SDK 默认（openai/_constants.py: DEFAULT_TIMEOUT =
# httpx.Timeout(60.0, connect=5.0)）：替换 SDK 前的代码未向 AsyncOpenAI 传
# timeout，底层即走该默认。实际生效的超时由调用侧 asyncio.wait_for
# （timeout_first_chunk / timeout_total）控制，httpx 层超时是底层兜底。
_DEFAULT_TIMEOUT = 60.0
_CONNECT_TIMEOUT = 5.0


def make_http_timeout() -> httpx.Timeout:
    """provider httpx client 的统一超时构造。"""
    return httpx.Timeout(_DEFAULT_TIMEOUT, connect=_CONNECT_TIMEOUT)
