"""`GatewayClient` — 全量 HTTP 客户端的方法面。

两类测试：
  - 请求契约（路径 / body / query / header）：用记录型替身锁住，不需要真实网关；
  - 生命周期（api_key header、close、async with）：直接查 httpx.AsyncClient 的实例状态。

协议里模型标识是单一引用词 `model_id`：客户端不得再发 legacy 的 `model` /
`provider` 字段——网关对它们静默忽略，发了等于切换失效。
"""

from __future__ import annotations

from typing import Any

import pytest

from wing_sdk.http_client import GatewayClient

pytestmark = pytest.mark.asyncio


class _FakeResponse:
    def __init__(self, payload: dict) -> None:
        self._payload = payload

    def raise_for_status(self) -> None:
        return None

    def json(self) -> dict:
        return self._payload


class _RecordingClient:
    """记录请求的最小替身（替代 httpx.AsyncClient）。"""

    def __init__(self) -> None:
        self.calls: list[dict[str, Any]] = []

    async def post(
        self, path: str, json: dict | None = None, headers: dict | None = None
    ) -> _FakeResponse:
        self.calls.append(
            {"method": "POST", "path": path, "json": json, "headers": headers or {}}
        )
        return _FakeResponse({"ok": True})

    async def get(self, path: str, params: dict | None = None) -> _FakeResponse:
        self.calls.append({"method": "GET", "path": path, "params": params})
        return _FakeResponse({"ok": True})


@pytest.fixture
def client() -> GatewayClient:
    c = GatewayClient("http://127.0.0.1:32523")
    recording = _RecordingClient()
    setattr(c, "_client", recording)
    return c


def _calls(client: GatewayClient) -> list[dict[str, Any]]:
    return getattr(client, "_client").calls


# ── 生命周期 ──────────────────────────────────────────────────


async def test_base_url_trailing_slash_stripped():
    c = GatewayClient("http://127.0.0.1:32523/")
    try:
        assert c.gateway_url == "http://127.0.0.1:32523"
    finally:
        await c.close()


async def test_api_key_becomes_bearer_header():
    c = GatewayClient("http://127.0.0.1:32523", api_key="sk-1")
    try:
        assert c._client.headers["authorization"] == "Bearer sk-1"
    finally:
        await c.close()


async def test_no_api_key_no_authorization_header():
    c = GatewayClient("http://127.0.0.1:32523")
    try:
        assert "authorization" not in c._client.headers
    finally:
        await c.close()


async def test_async_context_manager_closes():
    c = GatewayClient("http://127.0.0.1:32523")
    async with c as entered:
        assert entered is c
        assert c._client.is_closed is False
    assert c._client.is_closed is True


# ── Session 生命周期 ──────────────────────────────────────────


async def test_create_session_minimal_body(client: GatewayClient):
    await client.create_session()
    assert _calls(client) == [
        {"method": "POST", "path": "/api/session/create", "json": {}, "headers": {}}
    ]


async def test_create_session_full_body(client: GatewayClient):
    await client.create_session(
        template_name="tpl", workspace="/ws", agent={"model_id": "m"}, backend="memory"
    )
    assert _calls(client)[0]["json"] == {
        "template_name": "tpl",
        "workspace": "/ws",
        "agent": {"model_id": "m"},
        "backend": "memory",
    }


async def test_resume_fork_and_rewind(client: GatewayClient):
    await client.resume_session("S-1")
    await client.fork_session("S-1", "u-9")
    await client.rewind_session("S-1", "u-3")
    assert [(c["path"], c["json"]) for c in _calls(client)] == [
        ("/api/session/resume", {"session_id": "S-1"}),
        (
            "/api/session/fork",
            {"source_session_id": "S-1", "target_uuid": "u-9"},
        ),
        ("/api/session/rewind", {"session_id": "S-1", "target_uuid": "u-3"}),
    ]


async def test_subscribe_and_unsubscribe_carry_client_id_header(client: GatewayClient):
    await client.subscribe("S-1", "c-7")
    await client.unsubscribe("S-1", "c-7")
    assert _calls(client) == [
        {
            "method": "POST",
            "path": "/api/session/subscribe",
            "json": {"session_id": "S-1"},
            "headers": {"X-Client-Id": "c-7"},
        },
        {
            "method": "POST",
            "path": "/api/session/unsubscribe",
            "json": {"session_id": "S-1"},
            "headers": {"X-Client-Id": "c-7"},
        },
    ]


# ── 消息与操作 ────────────────────────────────────────────────


async def test_send_message_without_tool_call_id(client: GatewayClient):
    await client.send_message("S-1", "hi")
    assert _calls(client)[0]["json"] == {"session_id": "S-1", "content": "hi"}


async def test_send_message_with_tool_call_id(client: GatewayClient):
    await client.send_message("S-1", "yes", tool_call_id="call-1")
    assert _calls(client)[0]["json"] == {
        "session_id": "S-1",
        "content": "yes",
        "tool_call_id": "call-1",
    }


async def test_interrupt_and_compact(client: GatewayClient):
    await client.interrupt_session("S-1")
    await client.compact_session("S-1")
    await client.compact_session("S-1", instruction="keep decisions")
    assert [(c["path"], c["json"]) for c in _calls(client)] == [
        ("/api/session/interrupt", {"session_id": "S-1"}),
        ("/api/session/compact", {"session_id": "S-1"}),
        (
            "/api/session/compact",
            {"session_id": "S-1", "instruction": "keep decisions"},
        ),
    ]


async def test_update_session_sends_model_id_only():
    client = GatewayClient("http://127.0.0.1:32523")
    fake = _RecordingClient()
    setattr(client, "_client", fake)

    await client.update_session("S-1", model_id="ds-flash", title="renamed")

    assert fake.calls == [
        {
            "method": "POST",
            "path": "/api/session/update",
            "json": {"session_id": "S-1", "model_id": "ds-flash", "title": "renamed"},
            "headers": {},
        }
    ]
    body = fake.calls[0]["json"]
    assert "model" not in body and "provider" not in body


async def test_update_session_omits_none_valued_fields(client: GatewayClient):
    await client.update_session("S-1", thinking=False, yolo=True)
    # False 是「显式关闭」的合法值，不能被 is not None 之外的判断吞掉
    assert _calls(client)[0]["json"] == {
        "session_id": "S-1",
        "thinking": False,
        "yolo": True,
    }


async def test_update_session_sends_all_optional_fields(client: GatewayClient):
    await client.update_session(
        "S-1",
        model_id="m",
        agent="a",
        title="t",
        thinking=True,
        reasoning_effort="high",
        yolo=False,
        workspace="/ws",
    )
    assert _calls(client)[0]["json"] == {
        "session_id": "S-1",
        "model_id": "m",
        "agent": "a",
        "title": "t",
        "thinking": True,
        "reasoning_effort": "high",
        "yolo": False,
        "workspace": "/ws",
    }


# ── 查询与系统 ────────────────────────────────────────────────


async def test_query_endpoints(client: GatewayClient):
    await client.list_sessions()
    await client.get_session("S-1")
    await client.get_session_info("S-1")
    await client.get_branches("S-1")
    await client.health()
    await client.get_commands()
    await client.get_models()
    await client.get_agents()
    assert _calls(client) == [
        {"method": "GET", "path": "/api/session/list", "params": None},
        {"method": "GET", "path": "/api/session/get", "params": {"session_id": "S-1"}},
        {"method": "GET", "path": "/api/session/info", "params": {"session_id": "S-1"}},
        {
            "method": "GET",
            "path": "/api/session/branches",
            "params": {"session_id": "S-1"},
        },
        {"method": "GET", "path": "/api/health", "params": None},
        {"method": "GET", "path": "/api/commands", "params": None},
        {"method": "GET", "path": "/api/models", "params": None},
        {"method": "GET", "path": "/api/agents", "params": None},
    ]


async def test_system_endpoints(client: GatewayClient):
    await client.reload()
    await client.shutdown()
    assert [(c["path"], c["json"]) for c in _calls(client)] == [
        ("/api/system/reload", {}),
        ("/api/shutdown", {}),
    ]


async def test_response_payload_returned(client: GatewayClient):
    assert await client.health() == {"ok": True}
