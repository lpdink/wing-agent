# wing_gateway/server.py — Gateway 服务器

"""
Gateway 服务器——生命周期管理器 + EventBus 事件路由。

V2 实现（EventBus 模式）：
  - Gateway 不感知 session_id，只感知 client_id
  - Gateway 维护 {client_id: ws} 和 {ws: client_id} 两个 dict
  - Gateway subscribe EventBus，根据 EventTarget 转发给对应 ws
  - 断连时清理 Gateway 和 EventBus 的路由表

FastAPI app 创建和路由注册在 app.py 中完成（App Factory 模式）。
"""

from __future__ import annotations

import asyncio
import socket
import sys
from datetime import datetime, timezone

from fastapi import WebSocket
import uvicorn

from wing.common.logger import log
from wing.config import AuthConfig, load_config
from wing.event import WingEvent
from wing.event_bus import event_bus
from wing.runtime import WingRuntime

from .app import create_app

DEFAULT_PORT = 32523


def _check_port_available(host: str, port: int) -> bool:
    """检查端口是否可用，不可用时返回 False。

    使用 connect 而非 bind 来检测——避免 TIME_WAIT 误报。
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.settimeout(1)
            s.connect((host, port))
            # 能连上说明端口被占用
            return False
        except (OSError, socket.timeout):
            # 连不上说明端口空闲
            return True


class GatewayServer:
    """Gateway 服务器——生命周期管理器。

    持有 runtime、client 映射、FastAPI app。
    负责 start/stop 和 EventBus 事件路由。
    """

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = DEFAULT_PORT,
    ) -> None:
        self.host = host
        self.port = port
        self.runtime = WingRuntime()
        self._client_to_ws: dict[str, WebSocket] = {}  # client_id → ws
        self._ws_to_client: dict[WebSocket, str] = {}  # ws → client_id
        self._app = create_app(self)
        self._server: uvicorn.Server | None = None
        self._server_task: asyncio.Task[None] | None = None
        self._started_at = datetime.now(timezone.utc)

    @property
    def started_at(self) -> datetime:
        """Gateway 启动时间（UTC）。"""
        return self._started_at

    @property
    def uptime(self) -> int:
        """Gateway 运行时长（秒）。"""
        return int((datetime.now(timezone.utc) - self._started_at).total_seconds())

    @property
    def auth_config(self) -> AuthConfig:
        """当前鉴权配置（每次读取最新单例，热重载后立即生效）。"""
        return load_config().gateway.auth

    @property
    def clients(self) -> dict[str, WebSocket]:
        """client_id → WebSocket 映射，供 routes 访问。"""
        return self._client_to_ws

    @property
    def ws_to_clients(self) -> dict[WebSocket, str]:
        """WebSocket → client_id 映射，供 routes 访问。"""
        return self._ws_to_client

    def start(self) -> None:
        """启动服务器（阻塞）。"""
        if not _check_port_available(self.host, self.port):
            print(f"❌ 端口 {self.port} 已被占用，请指定其他端口或释放占用。")
            sys.exit(1)

        # Subscribe EventBus
        event_bus.subscribe(self._on_event)

        print(f"🚀 Gateway 启动于 {self.host}:{self.port}")
        uvicorn.run(
            self._app,
            host=self.host,
            port=self.port,
            log_config=None,
        )

    async def start_async(self, port: int | None = None) -> int:
        """异步启动服务器（用于测试）。返回实际使用的端口。"""
        actual_port = port or self.port
        if not _check_port_available(self.host, actual_port):
            raise RuntimeError(f"端口 {actual_port} 已被占用")

        self.port = actual_port

        config = uvicorn.Config(
            self._app,
            host=self.host,
            port=self.port,
            log_config=None,
        )
        self._server = uvicorn.Server(config)
        self._server_task = asyncio.create_task(self._server.serve())

        # 等待服务器启动
        while not self._server.started:
            await asyncio.sleep(0.05)

        # Subscribe EventBus
        event_bus.subscribe(self._on_event)

        log.info(f"Gateway async started on {self.host}:{self.port}")
        return self.port

    async def stop_async(self) -> None:
        """异步停止服务器（用于测试）。"""
        event_bus.unsubscribe(self._on_event)
        if self._server:
            self._server.should_exit = True
            if self._server_task:
                await self._server_task
                self._server_task = None
            self._server = None
        log.info("Gateway stopped")

    def _on_event(self, event: WingEvent) -> None:
        """EventBus subscriber callback：根据 EventTarget 路由事件到 ws。

        同步回调，内部 asyncio.create_task 调度异步 send。
        """
        target = event.target
        if target is None:
            return

        if target.scope == "global":
            # 发给所有 ws
            for ws in list(self._client_to_ws.values()):
                try:
                    asyncio.get_running_loop().create_task(
                        self._send_text(ws, event.model_dump_json())
                    )
                except RuntimeError:
                    pass

        elif target.scope == "client":
            # 发给指定 client_ids 的 ws
            for cid in target.client_ids:
                ws = self._client_to_ws.get(cid)
                if ws is not None:
                    try:
                        asyncio.get_running_loop().create_task(
                            self._send_text(ws, event.model_dump_json())
                        )
                    except RuntimeError:
                        pass

    async def _send_text(self, ws: WebSocket, data: str) -> None:
        """异步发送文本到 ws。"""
        try:
            await ws.send_text(data)
        except Exception as e:
            log.error(f"Failed to send to client: {e}")
