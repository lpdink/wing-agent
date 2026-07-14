# wing_gateway/routes/ws.py — WebSocket 端点

"""WebSocket handler——从 server.py 提取，逻辑不变。

V2 实现（EventBus 模式）：
  - Gateway 不感知 session_id，只感知 client_id
  - Gateway 维护 {client_id: ws} 和 {ws: client_id} 两个 dict
  - Gateway subscribe EventBus，根据 EventTarget 转发给对应 ws
  - 断连时清理 Gateway 和 EventBus 的路由表
"""

from __future__ import annotations

import json
import uuid

from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from wing.common.logger import log
from wing.event import ErrorEvent
from wing.event_bus import event_bus

from wing.gateway.protocol import ClientRequest, ConnectResponse

router = APIRouter()


@router.websocket("/ws")
async def handle_ws(ws: WebSocket) -> None:
    """处理 WebSocket 连接。"""
    await ws.accept()

    server = ws.app.state.server

    # 1. 生成 client_id
    client_id = uuid.uuid4().hex

    # 2. 注册 client_id ↔ ws 映射（不创建 session，不 route_attach）
    server.clients[client_id] = ws
    server.ws_to_clients[ws] = client_id

    # 3. 推送连接成功 + client_id
    await ws.send_json(ConnectResponse(client_id=client_id).model_dump())
    log.info(f"Client connected: {client_id}")

    # 4. 消息路由循环
    try:
        while True:
            data = await ws.receive_text()
            try:
                req = ClientRequest(**json.loads(data))
                await server.runtime.post(
                    content=req.content,
                    request_id=req.request_id,
                    session_id=req.session_id,
                    client_id=client_id,
                )
            except Exception as e:
                log.error(f"Failed to handle request: {e}")
                await ws.send_text(ErrorEvent(message=str(e)).model_dump_json())
    except WebSocketDisconnect:
        pass
    finally:
        # 断连：清理映射和路由表
        server.clients.pop(client_id, None)
        server.ws_to_clients.pop(ws, None)
        event_bus.route_detach_client(client_id)
        log.info(f"Client disconnected: {client_id}")
