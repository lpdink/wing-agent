"""``wing_probe.toolhost`` 组件自测（aiohttp 桩网关，不起真网关、不碰产品代码）。

桩网关在**同一端口**同时提供 ``POST /api/tools/register`` 与 ``/ws``——``ToolHost``
从 ``gateway_url`` 推导 WS 地址（``ws_url()`` + ``?client_id=``），两个端口会破坏这条
推导本身（那时自测就不是在测真实装配路径了）。

覆盖面：注册请求的线上形状（path / header / body 字段）、handshake、dispatch 往返
（async / sync handler、未知工具、handler 抛错）、调用留档与 ``wait_for_call``、``close``
的幂等与"取消挂起 dispatch"语义。
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator
from typing import Any

import pytest
import pytest_asyncio
from aiohttp import WSMsgType, web

from wing_probe import RemoteCall, ToolHost, ToolHostError

CLIENT_ID = "stub-host"


class StubGateway:
    """桩网关：注册端点 + WS 端点（同一端口/同一 app）。"""

    def __init__(self) -> None:
        self.runner: web.AppRunner | None = None
        self.port = 0
        self.registrations: list[dict[str, Any]] = []
        self.ws_query: dict[str, str] = {}
        self.frames: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
        self.host_ws: web.WebSocketResponse | None = None
        self.ws_connected = asyncio.Event()

    async def start(self) -> StubGateway:
        app = web.Application()
        app.router.add_post("/api/tools/register", self._register)
        app.router.add_get("/ws", self._ws)
        self.runner = web.AppRunner(app, access_log=None)
        await self.runner.setup()
        site = web.TCPSite(self.runner, "127.0.0.1", 0)
        await site.start()
        server = getattr(site, "_server", None)
        sockets = getattr(server, "sockets", None)
        assert sockets, "TCPSite 尚未绑定端口"
        self.port = int(sockets[0].getsockname()[1])
        return self

    async def stop(self) -> None:
        if self.runner is not None:
            await self.runner.cleanup()
            self.runner = None

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    async def _register(self, request: web.Request) -> web.Response:
        body = await request.json()
        self.registrations.append({"headers": dict(request.headers), "body": body})
        refs = [f"{CLIENT_ID}.{tool['name']}" for tool in body["tools"]]
        return web.json_response({"ok": True, "registered": refs})

    async def _ws(self, request: web.Request) -> web.WebSocketResponse:
        self.ws_query = dict(request.query)
        ws = web.WebSocketResponse(max_msg_size=16 * 1024 * 1024)
        await ws.prepare(request)
        self.host_ws = ws
        await ws.send_json({"type": "connected", "client_id": CLIENT_ID})
        self.ws_connected.set()
        try:
            async for message in ws:
                if message.type == WSMsgType.TEXT:
                    self.frames.put_nowait(json.loads(message.data))
        finally:
            self.host_ws = None
        return ws

    async def send_call(
        self, call_id: str, name: str, arguments: dict[str, Any] | None = None
    ) -> None:
        assert self.host_ws is not None, "host 尚未连接"
        await self.host_ws.send_json(
            {
                "type": "tool_call_request",
                "call_id": call_id,
                "name": name,
                "arguments": arguments or {},
            }
        )

    async def next_frame(self, timeout: float = 5.0) -> dict[str, Any]:
        return await asyncio.wait_for(self.frames.get(), timeout)

    def pending_frames(self) -> int:
        return self.frames.qsize()


@pytest_asyncio.fixture
async def stub_gateway() -> AsyncIterator[StubGateway]:
    stub = await StubGateway().start()
    try:
        yield stub
    finally:
        await stub.stop()


async def _echo(token: str) -> str:
    return f"echo:{token}"


def _upper(text: str) -> str:
    return text.upper()


async def _boom(**_: Any) -> str:
    raise ValueError("kaboom")


@pytest.mark.asyncio
async def test_registration_uses_public_protocol(stub_gateway: StubGateway) -> None:
    """握手 + 注册：`?client_id=`、`X-Client-Id`、规格字段与响应留档。"""
    host = ToolHost(CLIENT_ID, stub_gateway.url, started_at=0.0)
    host.add_tool(
        "Echo",
        _echo,
        description="Echo a token.",
        params=[{"name": "token", "type": "string", "description": "token to echo"}],
    )
    host.add_tool("Aliased", _upper, llm_name="AliasName")
    try:
        await host.start()

        assert stub_gateway.ws_query == {"client_id": CLIENT_ID}
        assert len(stub_gateway.registrations) == 1, stub_gateway.registrations
        registration = stub_gateway.registrations[0]
        assert registration["headers"]["X-Client-Id"] == CLIENT_ID
        assert registration["body"] == {
            "tools": [
                {
                    "name": "Echo",
                    "description": "Echo a token.",
                    "params": [
                        {
                            "name": "token",
                            "type": "string",
                            "description": "token to echo",
                        }
                    ],
                },
                {
                    "name": "Aliased",
                    "description": "",
                    "params": [],
                    "llm_name": "AliasName",
                },
            ]
        }, registration["body"]
        assert host.registered == [f"{CLIENT_ID}.Echo", f"{CLIENT_ID}.Aliased"]
        assert host.connected is True
    finally:
        await host.close()


@pytest.mark.asyncio
async def test_dispatch_round_trip_and_error_paths(stub_gateway: StubGateway) -> None:
    """dispatch：async / sync handler 往返、未知工具与 handler 异常都是 is_error 帧。"""
    host = ToolHost(CLIENT_ID, stub_gateway.url, started_at=0.0)
    host.add_tool("Echo", _echo)
    host.add_tool("Upper", _upper)
    host.add_tool("Boom", _boom)
    try:
        await host.start()

        await stub_gateway.send_call("call-1", "Echo", {"token": "a"})
        assert await stub_gateway.next_frame() == {
            "type": "tool_call_result",
            "call_id": "call-1",
            "result": "echo:a",
            "is_error": False,
        }

        await stub_gateway.send_call("call-2", "Upper", {"text": "x"})
        assert (await stub_gateway.next_frame())["result"] == "X"

        await stub_gateway.send_call("call-3", "Missing", {})
        unknown = await stub_gateway.next_frame()
        assert unknown["is_error"] is True
        assert "unknown tool: Missing" in unknown["result"]

        await stub_gateway.send_call("call-4", "Boom", {})
        failed = await stub_gateway.next_frame()
        assert failed["is_error"] is True
        assert "kaboom" in failed["result"]

        assert [call.call_id for call in host.calls] == [
            "call-1",
            "call-2",
            "call-3",
            "call-4",
        ]
        assert host.calls[0] == RemoteCall(
            call_id="call-1", name="Echo", arguments={"token": "a"}, at=host.calls[0].at
        )
        assert [entry["call_id"] for entry in host.sent_results] == [
            "call-1",
            "call-2",
            "call-3",
            "call-4",
        ]
        assert [entry["is_error"] for entry in host.sent_results] == [
            False,
            False,
            True,
            True,
        ]
    finally:
        await host.close()


@pytest.mark.asyncio
async def test_wait_for_call_is_idempotent_and_reports_timeout(
    stub_gateway: StubGateway,
) -> None:
    """``wait_for_call``：早到不漏（幂等）、未到超时给可读错误。"""
    host = ToolHost(CLIENT_ID, stub_gateway.url, started_at=0.0)
    host.add_tool("Echo", _echo)
    try:
        await host.start()

        waiter = asyncio.create_task(host.wait_for_call("Echo", timeout=5.0))
        await asyncio.sleep(0)  # 让 waiter 先挂上条件变量
        await stub_gateway.send_call("call-1", "Echo", {"token": "b"})
        call = await waiter
        assert (call.call_id, call.arguments) == ("call-1", {"token": "b"})
        assert await host.wait_for_call("Echo", timeout=1.0) == call  # 早到不漏

        with pytest.raises(ToolHostError) as failure:
            await host.wait_for_call("Missing", timeout=0.2)
        assert "recorded calls: ['Echo']" in str(failure.value)
    finally:
        await host.close()


@pytest.mark.asyncio
async def test_close_cancels_hanging_dispatch_and_is_idempotent(
    stub_gateway: StubGateway,
) -> None:
    """``close``：取消挂起 dispatch（不回帧）、幂等、连接状态翻转。"""
    gate = asyncio.Event()

    async def hang(**_: Any) -> str:
        await gate.wait()
        return "never"

    host = ToolHost(CLIENT_ID, stub_gateway.url, started_at=0.0)
    host.add_tool("Hang", hang)
    try:
        await host.start()
        await stub_gateway.send_call("call-1", "Hang", {})
        await host.wait_for_call("Hang", timeout=5.0)

        await host.close()
        await host.close()  # 幂等

        assert host.connected is False
        assert host.sent_results == [], "挂起的 dispatch 不得在关闭后回帧"
        assert stub_gateway.pending_frames() == 0
    finally:
        await host.close()
