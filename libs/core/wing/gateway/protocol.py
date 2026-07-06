# wing_gateway/protocol.py — 消息协议

"""
Gateway 的消息协议——前端和 Gateway 之间的通信格式。

V2 协议：Gateway 注入 client_id，不感知 session_id 业务语义。
ClientRequest 的 session_id 由前端传入（前端知道自己的 session）。
Gateway 只做 client_id 注入和 EventTarget 路由。
"""

from __future__ import annotations

import uuid

from pydantic import BaseModel, Field


# ============================================================
# 连接响应（Gateway → 前端，连接建立时）
# ============================================================


class ConnectResponse(BaseModel):
    """连接建立后的第一个消息——携带服务端分配的 session_id 和 client_id。

    前端收到后保存 session_id 和 client_id。
    - session_id：用于后续 ClientRequest 指定目标 session
    - client_id：前端可保存但不需要主动传递（Gateway 自动注入）

    /new 等命令会产生新 session_id，前端需要更新。
    """

    type: str = "connected"
    session_id: str
    client_id: str  # Gateway 生成的 client 标识


# ============================================================
# 入站消息（前端 → Gateway）
# ============================================================


class ClientRequest(BaseModel):
    """前端发来的请求——Gateway 注入 client_id 后转给 SM.post()。

    session_id: 前端指定目标 session（必填）
    content: 消息内容或魔术命令
    silent: 静默请求：不 emit DeliveredEvent 和 SystemEvent
    request_id: 前端用来解 Promise
    """

    request_id: str = Field(default_factory=lambda: uuid.uuid4().hex)
    session_id: str  # 指定目标 session（必填）
    content: str  # 消息内容或魔术命令
    silent: bool = False  # 静默请求
