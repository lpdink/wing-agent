# wing_gateway/routes/ws.py — WebSocket 端点

"""WebSocket handler——从 server.py 提取，逻辑不变。

V2 实现（EventBus 模式）：
  - Gateway 不感知 session_id，只感知 client_id
  - Gateway 维护 {client_id: ws} 和 {ws: client_id} 两个 dict
  - Gateway subscribe EventBus，根据 EventTarget 转发给对应 ws
  - 断连时清理 Gateway 和 EventBus 的路由表

远程工具扩展：
  - tool_runtime 角色连接时自选 client_id（作为工具 namespace），冲突拒连
  - 入站帧按类型分流：含 call_id 的是工具调用结果（→ RemoteToolManager），
    其余按既有 ClientRequest 处理（向后兼容）
  - 断连时 fail_client：在途调用立即失败 + 注销该 client 的远程工具
"""

from __future__ import annotations

import json
import uuid

from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from wing.common.logger import log
from wing.event import ErrorEvent
from wing.event_bus import event_bus

from wing.gateway.auth import ROLE_TOOL_RUNTIME, extract_key_from_ws
from wing.gateway.protocol import ClientRequest, ConnectResponse, ToolCallResult

router = APIRouter()


@router.websocket("/ws")
async def handle_ws(ws: WebSocket) -> None:
    """处理 WebSocket 连接。"""
    server = ws.app.state.server

    # 0. 鉴权（accept 之前）
    auth_config = server.auth_config
    role: str | None = None
    if auth_config.enabled:
        key = extract_key_from_ws(ws)
        role = auth_config.verify(key) if key is not None else None
        if role is None:
            await ws.close(code=4001, reason="Unauthorized")
            return

    is_tool_host = role == ROLE_TOOL_RUNTIME
    manager = server.remote_tools

    # 1. 确定 client_id：
    #    tool_runtime 必须自选（作为工具 namespace）；admin/未鉴权维持服务端分配。
    declared_id = ws.query_params.get("client_id")
    if is_tool_host:
        if not declared_id:
            await ws.close(code=4009, reason="tool_runtime must declare client_id")
            return
        if declared_id in server.clients:
            await ws.close(
                code=4009, reason=f"client_id '{declared_id}' already in use"
            )
            return
        client_id = declared_id
    else:
        client_id = declared_id or uuid.uuid4().hex

    await ws.accept()

    # 2. 注册 client_id ↔ ws 映射（不创建 session，不 route_attach）
    server.clients[client_id] = ws
    server.ws_to_clients[ws] = client_id
    if is_tool_host:
        manager.attach(client_id, ws)

    # 3. 推送连接成功 + client_id
    await ws.send_json(ConnectResponse(client_id=client_id).model_dump())
    log.info(f"Client connected: {client_id} (role={role or 'no-auth'})")

    # 4. 消息路由循环
    try:
        while True:
            data = await ws.receive_text()
            try:
                payload = json.loads(data)
            except json.JSONDecodeError as e:
                log.error(f"Invalid JSON frame from {client_id}: {e}")
                continue

            try:
                if "call_id" in payload:
                    # 工具调用结果帧 → 远程调用管理器
                    result = ToolCallResult(**payload)
                    manager.resolve_result(
                        result.call_id, result.result, result.is_error
                    )
                else:
                    # 用户消息帧 → 既有路径（向后兼容）
                    req = ClientRequest(**payload)
                    await server.runtime.post(
                        content=req.content,
                        request_id=req.request_id,
                        session_id=req.session_id,
                        client_id=client_id,
                        tool_call_id=req.tool_call_id,
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
        if is_tool_host:
            # 在途调用立即失败 + 注销远程工具（敏锐检测断连）
            manager.fail_client(client_id, "connection closed")
        log.info(f"Client disconnected: {client_id}")
