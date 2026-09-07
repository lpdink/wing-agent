# wing_gateway/routes/ws.py — WebSocket 端点

"""WebSocket handler——从 server.py 提取。

V2 实现（EventBus 模式）：
  - Gateway 不感知 session_id，只感知 client_id
  - Gateway 维护 {client_id: ws} 和 {ws: client_id} 两个 dict
  - Gateway subscribe EventBus，根据 EventTarget 转发给对应 ws
  - 断连时清理 Gateway 和 EventBus 的路由表

远程工具扩展：
  - client_id 自选与权限解耦：任何客户端都可经 ?client_id=<id> 自定义身份
    （面向未来 UX——用户需知道"远程是谁"以分配工具）。先来先得到，抢占式，
    全量唯一性校验（冲突拒连），不区分角色。声明了 client_id 即 attach 到
    RemoteToolManager（具备注册工具资格）；未声明则服务端分配 uuid（纯前端）。
  - 角色只决定"还能做什么"：tool_runtime 是纯工具执行远端——不得投递用户
    消息、不接收事件；admin 不受限（既可注册工具又可订阅事件）。
  - 入站帧按 call_id 分流：含 call_id 的是工具调用结果（→ RemoteToolManager，
    带归属校验），其余按 ClientRequest 处理（向后兼容）。
  - 断连时 fail_client：在途调用立即失败 + 注销该 client 的远程工具。
"""

from __future__ import annotations

import json
import uuid

from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from wing.common.logger import log
from wing.event import ErrorEvent, wire_dump
from wing.event_bus import event_bus

from wing.gateway.auth import ROLE_TOOL_RUNTIME, extract_key_from_ws
from wing.gateway.protocol import ClientRequest, ConnectResponse, ToolCallResult
from wing.tool_registry import DEFAULT_NAMESPACE

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
    declared_id = ws.query_params.get("client_id")

    # 1. 确定 client_id（与权限解耦）：
    #    - tool_runtime 必须声明（它靠 client_id 注册工具）；
    #    - 任何角色声明的 client_id 都走同一套唯一性校验（先来先得到）；
    #    - 未声明者服务端分配 uuid（纯前端，不 attach）。
    if declared_id:
        if declared_id == DEFAULT_NAMESPACE:
            # 'default' 是内置工具命名空间——若允许占用，断连注销会清空全局
            # 内置工具表。保留字，注册时即拒。（核心终将不持有内置工具，届时可放开。）
            await ws.close(code=4009, reason="client_id 'default' is reserved")
            return
        if declared_id in server.clients:
            await ws.close(
                code=4009, reason=f"client_id '{declared_id}' already in use"
            )
            return
        client_id = declared_id
    else:
        if is_tool_host:
            await ws.close(code=4009, reason="tool_runtime must declare client_id")
            return
        client_id = uuid.uuid4().hex

    # 2. 预留 client_id ↔ ws 映射。校验与登记之间无 await，单线程事件循环下
    #    原子，消除 TOCTOU（并发声明同一 id 时后者必撞上已登记项而被拒）。
    server.clients[client_id] = ws
    server.ws_to_clients[ws] = client_id

    try:
        await ws.accept()

        # 声明了 client_id → attach（具备注册工具资格）。tool_runtime 不收事件。
        if declared_id:
            manager.attach(client_id, ws, receives_events=not is_tool_host)

        # 3. 推送连接成功 + client_id
        await ws.send_json(ConnectResponse(client_id=client_id).model_dump())
        log.info(f"Client connected: {client_id} (role={role or 'no-auth'})")

        # 4. 消息路由循环
        while True:
            data = await ws.receive_text()
            try:
                payload = json.loads(data)
            except json.JSONDecodeError as e:
                log.error(f"Invalid JSON frame from {client_id}: {e}")
                continue

            try:
                if "call_id" in payload:
                    # 工具调用结果帧 → 远程调用管理器（带 client 归属校验）
                    result = ToolCallResult(**payload)
                    manager.resolve_result(
                        client_id, result.call_id, result.result, result.is_error
                    )
                else:
                    # 用户消息帧。tool_runtime 是纯工具执行远端，禁止投递。
                    if is_tool_host:
                        await ws.send_text(
                            json.dumps(
                                wire_dump(
                                    ErrorEvent(
                                        message="tool_runtime role cannot send user messages"
                                    )
                                )
                            )
                        )
                        continue
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
                await ws.send_text(json.dumps(wire_dump(ErrorEvent(message=str(e)))))
    except WebSocketDisconnect:
        pass
    finally:
        # 断连：清理映射和路由表
        server.clients.pop(client_id, None)
        server.ws_to_clients.pop(ws, None)
        event_bus.route_detach_client(client_id)
        if manager.is_attached(client_id):
            # 在途调用立即失败 + 注销远程工具（敏锐检测断连）
            manager.fail_client(client_id, "connection closed")
        log.info(f"Client disconnected: {client_id}")
