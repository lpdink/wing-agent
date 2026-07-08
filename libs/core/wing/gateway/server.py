# wing_gateway/server.py — WebSocket 服务器

"""
Gateway 的 WebSocket 服务器。

V2 实现（EventBus 模式）：
  - Gateway 不感知 session_id，只感知 client_id
  - Gateway 维护 {client_id: ws} 和 {ws: client_id} 两个 dict
  - Gateway subscribe EventBus，根据 EventTarget 转发给对应 ws
  - 断连时清理 Gateway 和 EventBus 的路由表

核心流程：
  1. 前端连接 ws:// → Gateway 生成 client_id → 推送 ConnectResponse(session_id)
  2. 前端发 ClientRequest → Gateway 注入 client_id → 转给 SM.post()
  3. EventBus emit → Gateway subscriber callback → 根据 EventTarget 路由到 ws
"""

from __future__ import annotations

import asyncio
import json
import socket
import sys
import uuid

from fastapi import FastAPI, WebSocket, WebSocketDisconnect
import uvicorn

from wing.common.logger import log
from wing.event import ErrorEvent, NewSessionEvent, WingEvent
from wing.event_bus import event_bus
from wing.runtime import WingRuntime

from .protocol import ClientRequest, ConnectResponse

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
    """WebSocket 服务器——Gateway 的网络层。

    Gateway 不感知 session_id。只维护 client_id ↔ ws 映射。
    事件路由由 EventBus 的路由表 + EventTarget 决定。
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
        self._app = FastAPI()
        self._app.websocket("/ws")(self._handle_ws)
        self._server: uvicorn.Server | None = None
        self._server_task: asyncio.Task[None] | None = None

    def start(self) -> None:
        """启动服务器（阻塞）。"""
        if not _check_port_available(self.host, self.port):
            print(f"❌ 端口 {self.port} 已被占用，请指定其他端口或释放占用。")
            sys.exit(1)

        # Subscribe EventBus
        event_bus.subscribe(self._on_event)

        print(f"🚀 Gateway 启动于 ws://{self.host}:{self.port}/ws")
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

        # 处理 session 切换：更新路由表
        # NewSessionEvent 表示 client 从 old_session 切换到 new_session
        if isinstance(event, NewSessionEvent):
            old_session_id = event.session_id
            new_session_id = event.new_session_id
            if old_session_id is None:
                return
            if target.scope == "client":
                for cid in target.client_ids:
                    # 更新路由：移除旧 session，添加新 session
                    event_bus.route_detach(cid, old_session_id)
                    event_bus.route_attach(cid, new_session_id)
                    log.info(
                        f"Updated routing for client {cid}: {old_session_id} → {new_session_id}"
                    )

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

    async def _handle_ws(self, ws: WebSocket) -> None:
        """处理 WebSocket 连接。"""
        await ws.accept()

        # 1. 生成 client_id
        client_id = uuid.uuid4().hex

        # 2. 创建初始 session（使用默认模板）
        # 从 URL query 提取 workspace（TUI 传递的启动目录）
        workspace = ws.query_params.get("workspace")
        session = self.runtime.create_session(workspace=workspace)

        # 3. 注册 client_id ↔ ws 映射 + EventBus 路由表
        self._client_to_ws[client_id] = ws
        self._ws_to_client[ws] = client_id
        event_bus.route_attach(client_id, session.session_id)

        # 4. 推送连接成功 + session_id + client_id
        await ws.send_json(
            ConnectResponse(
                session_id=session.session_id, client_id=client_id
            ).model_dump()
        )
        log.info(f"Client connected: {client_id}, session: {session.session_id}")

        # 5. 消息路由循环
        try:
            while True:
                data = await ws.receive_text()
                try:
                    req = ClientRequest(**json.loads(data))
                    await self.runtime.post(
                        content=req.content,
                        request_id=req.request_id,
                        session_id=req.session_id,
                        client_id=client_id,
                        silent=req.silent,
                    )
                except Exception as e:
                    log.error(f"Failed to handle request: {e}")
                    await ws.send_text(ErrorEvent(message=str(e)).model_dump_json())
        except WebSocketDisconnect:
            pass
        finally:
            # 断连：清理映射和路由表
            self._client_to_ws.pop(client_id, None)
            self._ws_to_client.pop(ws, None)
            event_bus.route_detach_client(client_id)
            log.info(f"Client disconnected: {client_id}")
