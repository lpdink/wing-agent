# wing/gateway/routes/ws.py — WebSocket 端点

"""WebSocket 端点：连接身份（client_id）确定、上行帧路由与断连清理。

帧路由的两条脊柱见 gateway/server.py 的模块 docstring（client_id 表 + EventBus）。
本端点另有远程工具扩展面：

  - client_id 自选与权限解耦：任何客户端都可经 ?client_id=<id> 自定义身份
    （面向未来 UX——用户需知道"远程是谁"以分配工具）。先来先得到，抢占式，
    全量唯一性校验（冲突拒连），不区分角色。声明了 client_id 即 attach 到
    RemoteToolManager（具备注册工具资格）；未声明则服务端分配 uuid（纯前端）。
  - 角色只决定"还能做什么"：tool_runtime 是纯工具执行远端——不得投递用户
    消息、不接收事件；admin 不受限（既可注册工具又可订阅事件）。
  - 入站帧按 call_id 分流：含 call_id 的是工具调用结果（→ RemoteToolManager，
    带归属校验），其余按 ClientRequest 处理（向后兼容）。
  - 断连时 fail_client：在途调用立即失败 + 注销该 client 的远程工具。
  - setup mode（04）：**accept 之前**直接以 1013 关闭——配置不可用时 WS 没有任何
    可用功能，客户端看到的是握手失败而不是连上就断。
"""

from __future__ import annotations

import json
import uuid

from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from wing.common.logger import log
from wing.event import ErrorEvent, wire_dump

from wing.gateway.auth import ROLE_TOOL_RUNTIME, extract_key_from_ws
from wing.gateway.protocol import ClientRequest, ConnectResponse, ToolCallResult
from wing.tool_registry import DEFAULT_NAMESPACE

router = APIRouter()


@router.websocket("/ws")
async def handle_ws(ws: WebSocket) -> None:
    """处理 WebSocket 连接。"""
    server = ws.app.state.server

    # 0. setup mode：握手即拒（**accept 之前** close ⇒ 客户端看到的是握手失败，
    #    而不是「连上了又被断」）。setup mode 下 WS 没有任何可用功能——修复全在
    #    HTTP 设置端点上（守门白名单），loopback 与非 loopback 一视同仁；
    #    真正的来源策略由 HTTP 面的 AuthMiddleware 承担（§8.4）。
    if server.in_setup_mode:
        await ws.close(code=1013, reason="setup_mode")
        return

    # 1. 鉴权（必须在 accept 之前）
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
            # 内置工具表。保留字，注册时即拒。
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

        # attach = 具备注册工具资格；tool_runtime 不收事件。
        if declared_id:
            manager.attach(client_id, ws, receives_events=not is_tool_host)

        await ws.send_json(ConnectResponse(client_id=client_id).model_dump())
        log.info(f"Client connected: {client_id} (role={role or 'no-auth'})")

        while True:
            data = await ws.receive_text()
            try:
                payload = json.loads(data)
            except json.JSONDecodeError as e:
                log.error(f"Invalid JSON frame from {client_id}: {e}")
                continue

            try:
                if "call_id" in payload:
                    result = ToolCallResult(**payload)
                    manager.resolve_result(
                        client_id, result.call_id, result.result, result.is_error
                    )
                else:
                    # tool_runtime 是纯工具执行远端，禁止投递用户消息。
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
        # 断连：清理映射、路由表与远程工具归属。与慢消费者回收（server.py
        # 的 _send_text → _recycle_client）共用同一个幂等入口——两条路径
        # 的清理动作不允许漂移。
        await server.drop_client(ws, reason="connection closed")
