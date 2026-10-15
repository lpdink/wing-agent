# wing/gateway/protocol/ws.py — WS 帧协议

"""WebSocket 帧模型——``/ws`` 连接期间的出站与入站消息。

出站：``ConnectResponse``（握手）、``ToolCallRequest``（远程工具调用请求）；
入站：``ClientRequest``（上行请求）、``ToolCallResult``（远程工具调用结果）。
"""

from __future__ import annotations

import uuid
from typing import Literal

from pydantic import BaseModel, Field


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
    tool_call_id: str | None = Field(
        default=None,
        description="回复某个 Ask 事件时携带其 tool_call_id，定向 resolve feedback waiter",
    )


class ToolCallRequest(BaseModel):
    """Gateway 经 WS 发给 tool host 的工具调用请求帧（出站）。

    tool host 执行后以 ToolCallResult（同 call_id）回传。call_id 由
    RemoteToolManager 生成，用于在 pending future 表中关联请求与响应。
    """

    type: Literal["tool_call_request"] = Field(
        default="tool_call_request", description="帧类型，固定为 'tool_call_request'"
    )
    call_id: str = Field(description="调用唯一 ID，结果帧据此关联")
    name: str = Field(description="工具注册名（不含 namespace 前缀）")
    arguments: dict = Field(default_factory=dict, description="工具调用参数")


class ToolCallResult(BaseModel):
    """tool host 经 WS 回传的工具调用结果帧（入站）。"""

    type: Literal["tool_call_result"] = Field(
        default="tool_call_result", description="帧类型，固定为 'tool_call_result'"
    )
    call_id: str = Field(description="对应的调用 ID")
    result: str = Field(default="", description="工具执行结果文本")
    is_error: bool = Field(default=False, description="结果是否为错误")
