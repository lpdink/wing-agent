"""跨文件共用的测试辅助：进程内假 Provider 的装配与离线对账的固定参数。

只放装配：进程内 provider 的生命周期上下文、一次 ``chat/completions`` 的请求体，以及
``stream_frames`` 的固定 ``completion_id`` / ``created``。测试自己的语义辅助（帧序、
解析、断言）留在各自的文件里。
"""

from __future__ import annotations

import contextlib
from collections.abc import AsyncIterator
from typing import Any

from wing_probe.provider.server import FakeProvider

COMPLETION_ID = "chatcmpl-probe-0"
"""离线对账用的固定 completion id（``stream_frames`` 的三个装配参数之一）。"""
CREATED = 1_700_000_000
"""同上，固定 ``created`` 时间戳。"""


@contextlib.asynccontextmanager
async def running_provider() -> AsyncIterator[FakeProvider]:
    """起一个进程内假 Provider（端口由 OS 分配），退出时停掉。"""
    provider = FakeProvider()
    await provider.start()
    try:
        yield provider
    finally:
        await provider.stop()


def request_body(model: str, **overrides: object) -> dict[str, Any]:
    """一次流式 ``chat/completions`` 请求体（``overrides`` 原样覆盖）。"""
    body: dict[str, Any] = {
        "model": model,
        "stream": True,
        "messages": [{"role": "user", "content": "hi"}],
    }
    body.update(overrides)
    return body
