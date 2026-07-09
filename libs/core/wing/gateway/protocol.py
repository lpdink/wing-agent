# wing_gateway/protocol.py — 消息协议

"""
Gateway 的消息协议——前端和 Gateway 之间的通信格式。

包含：
  - WS 协议：ConnectResponse、ClientRequest
  - HTTP 协议：所有 HTTP 端点的 Request/Response models

这些 Pydantic models 同时用于：
  - 请求/响应验证
  - OpenAPI schema 生成
  - Rust client 代码生成（通过 openapi-generator）
"""

from __future__ import annotations

import uuid

from pydantic import BaseModel, Field

from wing.event import AgentInfo, SessionInfo


# ============================================================
# WS 协议（WebSocket）
# ============================================================


class ConnectResponse(BaseModel):
    """WS 连接建立后的第一个消息——携带服务端分配的 client_id。

    前端收到后保存 client_id，后续 HTTP 请求通过 X-Client-Id header 传递。
    """

    type: str = Field(default="connected", description="消息类型，固定为 'connected'")
    client_id: str = Field(description="Gateway 分配的客户端唯一标识")


class ClientRequest(BaseModel):
    """前端通过 WS 发送的请求。

    Gateway 注入 client_id 后转给 WingRuntime.post()。
    """

    request_id: str = Field(
        default_factory=lambda: uuid.uuid4().hex,
        description="请求唯一 ID，用于前端关联响应",
    )
    session_id: str = Field(description="目标 session ID")
    content: str = Field(description="消息内容")
    silent: bool = Field(
        default=False, description="静默请求：不触发 DeliveredEvent 和 SystemEvent"
    )


# ============================================================
# HTTP Request Models
# ============================================================


class AgentOverride(BaseModel):
    """Agent 参数覆盖。所有字段可选，None 表示不覆盖（保留 template 值）。"""

    model: str | None = Field(default=None, description="覆盖模型名称")
    system_prompt: str | None = Field(default=None, description="替换系统提示词")
    append_system_prompt: str | None = Field(
        default=None, description="追加到系统提示词末尾"
    )
    tools: list[str] | None = Field(default=None, description="覆盖工具列表")
    max_turns: int | None = Field(default=None, description="Agent loop 最大轮数")
    effort: str | None = Field(
        default=None, description="Reasoning effort: low|medium|high"
    )


class CreateSessionRequest(BaseModel):
    """创建新 session 的请求体。"""

    template_name: str | None = Field(
        default=None, description="Agent 模板名称，None 使用默认模板"
    )
    workspace: str | None = Field(default=None, description="工作目录路径")
    agent: AgentOverride | None = Field(default=None, description="Agent 参数覆盖")


class ResumeSessionRequest(BaseModel):
    """恢复已有 session 的请求体。"""

    session_id: str = Field(description="要恢复的 session ID")


class ForkSessionRequest(BaseModel):
    """分叉 session 的请求体。"""

    source_session_id: str = Field(description="源 session ID")
    target_uuid: str = Field(description="分叉点消息的 UUID")


class SubscribeRequest(BaseModel):
    """订阅 session 事件的请求体。需要 X-Client-Id header。"""

    session_id: str = Field(description="要订阅的 session ID")


class UnsubscribeRequest(BaseModel):
    """取消订阅 session 事件的请求体。需要 X-Client-Id header。"""

    session_id: str = Field(description="要取消订阅的 session ID")


class SendMessageRequest(BaseModel):
    """向 session 发送消息的请求体。"""

    session_id: str = Field(description="目标 session ID")
    content: str = Field(description="消息内容")
    silent: bool = Field(
        default=False, description="静默发送：不触发 DeliveredEvent 和 SystemEvent"
    )


# ============================================================
# HTTP Response Models
# ============================================================


class CreateSessionResponse(BaseModel):
    """创建 session 的响应。"""

    session_id: str = Field(description="新创建的 session ID")
    template_name: str = Field(description="使用的模板名称")
    workspace: str | None = Field(default=None, description="工作目录路径")


class ResumeSessionResponse(BaseModel):
    """恢复 session 的响应。"""

    session_id: str = Field(description="恢复的 session ID")
    template_name: str | None = Field(default=None, description="使用的模板名称")
    workspace: str | None = Field(default=None, description="工作目录路径")


class ForkSessionResponse(BaseModel):
    """分叉 session 的响应。"""

    session_id: str = Field(description="新分叉出的 session ID")
    draft: str | None = Field(
        default=None, description="分叉点处的 draft 消息（如果有）"
    )


class OkResponse(BaseModel):
    """通用成功响应。"""

    ok: bool = Field(default=True, description="操作是否成功")


class SendMessageResponse(BaseModel):
    """发送消息的响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    request_id: str = Field(description="请求 ID，用于前端关联 agent 响应")


class SessionListResponse(BaseModel):
    """session 列表响应。"""

    sessions: list[SessionInfo] = Field(description="所有活跃 session 的摘要列表")


class SessionGetResponse(BaseModel):
    """session 详情响应。"""

    session_id: str = Field(description="session ID")
    name: str | None = Field(default=None, description="session 名称")
    template_name: str | None = Field(default=None, description="使用的模板名称")
    workspace: str | None = Field(default=None, description="工作目录路径")
    messages: list[dict] = Field(description="消息历史列表")
    agent: AgentInfo | None = Field(default=None, description="当前 agent 配置信息")


class HealthResponse(BaseModel):
    """健康检查响应。"""

    status: str = Field(default="ok", description="服务状态")
    version: str = Field(description="wing-agent 版本号")


class ErrorResponse(BaseModel):
    """错误响应。"""

    error: str = Field(description="错误类型")
    detail: str | None = Field(default=None, description="错误详细信息")
    session_id: str | None = Field(default=None, description="关联的 session ID")
    uuid: str | None = Field(default=None, description="关联的消息 UUID")
