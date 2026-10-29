"""跨文件共用的测试辅助：进程内假 Provider 的装配与请求体。

只放「同一段代码在多个测试文件里逐字重复」的东西——有真实语义的辅助仍归各自的文件。
"""

from __future__ import annotations

import contextlib
from collections.abc import AsyncIterator
from typing import Any

from wing_probe.provider.server import FakeProvider


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
