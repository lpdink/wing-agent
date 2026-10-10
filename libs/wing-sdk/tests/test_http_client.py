"""`GatewayClient.update_session` 的请求体契约（不需要真实网关）。

协议里模型标识是单一引用词 `model_id`：客户端不得再发 legacy 的 `model` /
`provider` 字段——网关对它们静默忽略，发了等于切换失效。
"""

from __future__ import annotations

from typing import Any

import pytest

from wing_sdk.http_client import GatewayClient


class _FakeResponse:
    def __init__(self, payload: dict) -> None:
        self._payload = payload

    def raise_for_status(self) -> None:
        return None

    def json(self) -> dict:
        return self._payload


class _FakeAsyncClient:
    """记录请求的最小替身（替代 httpx.AsyncClient）。"""

    def __init__(self) -> None:
        self.posts: list[dict[str, Any]] = []

    async def post(
        self, path: str, json: dict | None = None, headers: dict | None = None
    ) -> _FakeResponse:
        self.posts.append({"path": path, "json": json, "headers": headers or {}})
        return _FakeResponse({"ok": True})


@pytest.mark.asyncio
async def test_update_session_sends_model_id_only():
    client = GatewayClient("http://127.0.0.1:32523")
    fake = _FakeAsyncClient()
    setattr(client, "_client", fake)

    await client.update_session("S-1", model_id="ds-flash", title="renamed")

    assert fake.posts == [
        {
            "path": "/api/session/update",
            "json": {"session_id": "S-1", "model_id": "ds-flash", "title": "renamed"},
            "headers": {},
        }
    ]
    body = fake.posts[0]["json"]
    assert "model" not in body and "provider" not in body
