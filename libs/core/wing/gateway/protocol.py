# wing_gateway/protocol.py — 消息协议

"""
Gateway 的消息协议——前端和 Gateway 之间的通信格式。

包含：
  - WS 协议：ConnectResponse、ClientRequest
  - HTTP 协议：所有 HTTP 端点的 Request/Response models
"""

from __future__ import annotations

import uuid

from pydantic import BaseModel, Field

from wing.event import AgentInfo, SessionInfo


# ============================================================
# WS 协议（WebSocket）
# ============================================================


class ConnectResponse(BaseModel):
    """连接建立后的第一个消息——携带服务端分配的 client_id。

    前端收到后保存 client_id。
    session 生命周期通过 HTTP API 管理（create + subscribe），不再由 WS 连接自动创建。
    """

    type: str = "connected"
    client_id: str  # Gateway 生成的 client 标识


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


# ============================================================
# HTTP Request Models
# ============================================================


class CreateSessionRequest(BaseModel):
    template_name: str | None = None
    workspace: str | None = None


class ResumeSessionRequest(BaseModel):
    session_id: str


class ForkSessionRequest(BaseModel):
    source_session_id: str
    target_uuid: str


class SubscribeRequest(BaseModel):
    session_id: str


class UnsubscribeRequest(BaseModel):
    session_id: str


class SendMessageRequest(BaseModel):
    session_id: str
    content: str
    silent: bool = False


# ============================================================
# HTTP Response Models
# ============================================================


class CreateSessionResponse(BaseModel):
    session_id: str
    template_name: str
    workspace: str | None = None


class ResumeSessionResponse(BaseModel):
    session_id: str
    template_name: str | None = None
    workspace: str | None = None


class ForkSessionResponse(BaseModel):
    session_id: str
    draft: str | None = None


class OkResponse(BaseModel):
    ok: bool = True


class SendMessageResponse(BaseModel):
    ok: bool = True
    request_id: str


class SessionListResponse(BaseModel):
    sessions: list[SessionInfo]


class SessionGetResponse(BaseModel):
    session_id: str
    name: str | None = None
    template_name: str | None = None
    workspace: str | None = None
    messages: list[dict]
    agent: AgentInfo | None = None


class HealthResponse(BaseModel):
    status: str = "ok"
    version: str


class ErrorResponse(BaseModel):
    error: str
    detail: str | None = None
    session_id: str | None = None
    uuid: str | None = None
