"""网关慢消费者契约：写超时 / 写失败 → 回收连接与路由表。

背景（2026-09-13 实测）：一个读任务死亡的 `wing wait` 客户端让网关永久留下
一条僵尸连接——`handle_ws` 等不到断连（路由表项泄漏），而每个订阅事件都变成
一次失败投递尝试（30s 内 5,497 条 `Unexpected ASGI message` 日志，事件速率实测
244.5 events/s）。网关对"不再读取的客户端"没有任何反制。

本文件钉死：
- 单次写有上界（60s 常量），超时/失败都走同一条回收路径；
- 回收 = 注销两张映射表 + 清 EventBus 路由 + 主动关连接（关连接自身有上界）；
- 回收幂等（`handle_ws` 的 finally 与发送失败路径共用同一入口）；
- 回收后该 client 不再收到任何投递。
"""

from __future__ import annotations

import asyncio
import json
import time
from typing import TYPE_CHECKING, cast
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi import WebSocket
from fastapi.testclient import TestClient

from wing.config import ApiKeyEntry, AuthConfig
from wing.event import EventTarget, NoticeEvent
from wing.event_bus import event_bus

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

SESSION = "test-session"


def _as_ws(obj: object) -> WebSocket:
    """测试替身冒充 `WebSocket`（运行时是鸭子类型，类型上显式断言）。"""
    return cast("WebSocket", obj)


def _mock_config() -> MagicMock:
    """接住 `load_config()`：鉴权关闭（与既有 gateway 测试同款）。"""
    config = MagicMock()
    config.gateway.auth = AuthConfig(
        enabled=False, keys=[ApiKeyEntry(key="k", role="admin")]
    )
    return config


def _make_server() -> GatewayServer:
    """造一个 GatewayServer（WingRuntime / load_config 都 mock 掉）。"""
    with (
        patch("wing.gateway.server.WingRuntime") as MockRuntime,
        patch("wing.gateway.server.load_config") as mock_load_config,
    ):
        MockRuntime.return_value = MagicMock()
        mock_load_config.return_value = _mock_config()
        from wing.gateway.server import GatewayServer

        return GatewayServer()


@pytest.fixture(autouse=True)
def clean_event_bus():
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


class _HangingWs:
    """发送永不返回的客户端（慢/死消费者）。"""

    def __init__(self) -> None:
        self.close_calls: list[tuple[int, str]] = []

    async def send_text(self, data: str) -> None:
        await asyncio.Event().wait()  # 永不 set

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self.close_calls.append((code, reason))


class _FailingWs:
    """发送直接抛错的客户端（连接已死 / ASGI 层已关闭）。"""

    def __init__(self) -> None:
        self.close_calls: list[tuple[int, str]] = []

    async def send_text(self, data: str) -> None:
        raise RuntimeError(
            "Unexpected ASGI message 'websocket.send', after sending 'websocket.close'"
        )

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self.close_calls.append((code, reason))


def _register(server, ws, client_id: str = "slow-client") -> None:
    server.clients[client_id] = ws
    server.ws_to_clients[ws] = client_id


class TestSlowConsumerRecycle:
    @pytest.mark.timeout(10)
    @pytest.mark.asyncio
    async def test_write_timeout_recycles_client(self, monkeypatch: pytest.MonkeyPatch):
        monkeypatch.setattr("wing.gateway.server.WRITE_TIMEOUT_SECONDS", 0.05)
        server = _make_server()
        stub = _HangingWs()
        ws = _as_ws(stub)
        _register(server, ws, "slow-client")
        event_bus.route_attach("slow-client", SESSION)

        await server._send_text(ws, "{}")

        assert "slow-client" not in server.clients
        assert ws not in server.ws_to_clients
        assert "slow-client" not in event_bus.routing_table, "路由表项泄漏 = 继续投递"
        assert stub.close_calls == [(1013, "slow consumer")], "回收必须主动关连接"

    @pytest.mark.asyncio
    async def test_send_failure_recycles_client(self):
        server = _make_server()
        stub = _FailingWs()
        ws = _as_ws(stub)
        _register(server, ws, "dead-client")
        event_bus.route_attach("dead-client", SESSION)

        await server._send_text(ws, "{}")

        assert "dead-client" not in server.clients
        assert ws not in server.ws_to_clients
        assert "dead-client" not in event_bus.routing_table
        assert stub.close_calls == [(1013, "slow consumer")]

    @pytest.mark.asyncio
    async def test_recycle_is_idempotent(self):
        """发送失败紧接 `handle_ws` 的 finally —— 只允许一个赢家。"""
        server = _make_server()
        ws = _as_ws(_FailingWs())
        _register(server, ws, "dead-client")

        with patch.object(server.remote_tools, "fail_client") as fail_client:
            server.remote_tools.is_attached = lambda cid: True  # ty: ignore[invalid-assignment]
            first = await server.drop_client(ws, reason="send failed")
            second = await server.drop_client(ws, reason="connection closed")

        assert first == "dead-client"
        assert second is None, "第二次回收不得产生副作用"
        fail_client.assert_called_once()

    @pytest.mark.asyncio
    async def test_attached_client_is_failed_on_recycle(self):
        """远程工具宿主被回收时：在途调用立即失败 + 工具注销（归属回收）。"""
        server = _make_server()
        ws = _as_ws(_FailingWs())
        _register(server, ws, "tool-host")

        with patch.object(server.remote_tools, "fail_client") as fail_client:
            server.remote_tools.is_attached = lambda cid: True  # ty: ignore[invalid-assignment]
            assert (
                await server.drop_client(ws, reason="write timeout after 60s")
                == "tool-host"
            )

        fail_client.assert_called_once_with("tool-host", "write timeout after 60s")

    @pytest.mark.asyncio
    async def test_close_failure_does_not_block_recycle(self):
        """对端不读导致关闭本身卡住时，回收仍必须返回（独立上界）。"""

        class _StubbornWs(_FailingWs):
            async def close(self, code: int = 1000, reason: str = "") -> None:
                await asyncio.Event().wait()

        import wing.gateway.server as server_module

        server = _make_server()
        ws = _as_ws(_StubbornWs())
        _register(server, ws, "stubborn")
        with patch.object(server_module, "CLOSE_TIMEOUT_SECONDS", 0.05):
            await server._send_text(ws, "{}")

        assert "stubborn" not in server.clients

    @pytest.mark.asyncio
    async def test_no_delivery_after_recycle(self):
        """回收立即生效：注销后该 client 不再出现在投递列表里。"""
        server = _make_server()
        ws = _as_ws(_FailingWs())
        _register(server, ws, "slow-client")
        event_bus.route_attach("slow-client", SESSION)

        event = NoticeEvent(
            message="x", target=EventTarget(scope="client", client_ids=["slow-client"])
        )

        # 反证：回收前会投递。
        with patch.object(server, "_send_text", new_callable=AsyncMock) as spy:
            server._on_event(event)
            await asyncio.sleep(0)
        assert spy.await_count == 1, "回收前的对照必须真的投递"

        await server._send_text(ws, "{}")  # 触发回收

        with patch.object(server, "_send_text", new_callable=AsyncMock) as spy:
            server._on_event(event)
            await asyncio.sleep(0)
        assert spy.await_count == 0, "回收后不得再投递"

    @pytest.mark.asyncio
    async def test_payload_serialization_unchanged(self):
        """投递帧仍是 `wire_dump` 的形状（只加了有界等待，不改内容）。"""
        sent: list[str] = []

        class _RecordingWs:
            async def send_text(self, data: str) -> None:
                sent.append(data)

            async def close(self, code: int = 1000, reason: str = "") -> None:
                pass

        server = _make_server()
        ws = _as_ws(_RecordingWs())
        _register(server, ws, "ok-client")
        await server._send_text(ws, '{"a":1}')
        assert sent == ['{"a":1}']

    def test_write_timeout_is_a_constant(self):
        from wing.gateway import server as server_module

        assert server_module.WRITE_TIMEOUT_SECONDS == 60.0
        assert server_module.CLOSE_TIMEOUT_SECONDS == 5.0


# ============================================================
# 集成：真实 WS 断连仍走同一份清理
# ============================================================


def _wait_until(predicate, timeout: float = 3.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.01)
    return predicate()


@pytest.fixture
def ws_server():
    """TestClient + 真实 GatewayServer（含 WS 路由）。"""
    server = _make_server()
    with TestClient(server._app) as tc:
        yield tc, server


class TestDisconnectStillCleansUp:
    def test_ws_disconnect_clears_maps_and_routing(self, ws_server):
        tc, server = ws_server
        with tc.websocket_connect("/ws") as ws:
            handshake = json.loads(ws.receive_text())
            client_id = handshake["client_id"]
            assert client_id in server.clients
            event_bus.route_attach(client_id, SESSION)
        # 上下文退出 = 断连；服务端收尾由 handle_ws 的 finally 完成。
        assert _wait_until(lambda: client_id not in server.clients), "映射表未清理"
        assert client_id not in event_bus.routing_table, "路由表未清理"
        assert all(cid != client_id for cid in server.ws_to_clients.values())

    def test_declared_client_id_can_reconnect_after_recycle(self, ws_server):
        """回收不留残留状态：同一 client_id 可立即重连。"""
        tc, server = ws_server
        with tc.websocket_connect("/ws?client_id=my-host") as ws:
            assert json.loads(ws.receive_text())["client_id"] == "my-host"
        assert _wait_until(lambda: "my-host" not in server.clients)

        with tc.websocket_connect("/ws?client_id=my-host") as ws:
            assert json.loads(ws.receive_text())["client_id"] == "my-host"
