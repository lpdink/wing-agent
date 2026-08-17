# wing/provider/http.py
"""provider 层共享的 httpx 构造（两个协议同构）。"""

from __future__ import annotations

import httpx

# httpx 层超时是底层兜底——必须 > 应用层 timeout_total（默认 600s），
# 否则非流式调用（如压缩 LLM decode）会在 httpx 层被杀，先于 asyncio.wait_for
# 的 timeout_total 触发。设 1200s（2x timeout_total）确保不干扰应用层控制。
# 实际生效的超时由调用侧 asyncio.wait_for
# （timeout_first_chunk / timeout_total）控制。
_DEFAULT_TIMEOUT = 1200.0
_CONNECT_TIMEOUT = 5.0


def make_http_timeout() -> httpx.Timeout:
    """provider httpx client 的统一超时构造。"""
    return httpx.Timeout(_DEFAULT_TIMEOUT, connect=_CONNECT_TIMEOUT)
