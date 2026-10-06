# wing/gateway/protocol/__init__.py — 消息协议

"""Gateway 的消息协议——前端和 Gateway 之间的通信格式。

包含：
  - WS 协议：ConnectResponse、ClientRequest
  - HTTP 协议：所有 HTTP 端点的 Request/Response models

这些 Pydantic models 同时用于：
  - 请求/响应验证
  - OpenAPI schema 生成
  - Rust client 代码生成（通过 openapi-generator）

按端点族分四个子模块：``ws``（WS 帧）· ``session``（``/api/session/*`` 请求与响应）·
``system``（系统面：命令 / 模型 / agent / 重载 / 工具 / 健康 / 远程工具注册）·
``errors``（统一错误形状）。本 __init__ 是唯一公共入口——消费方 import 保持包根形态。"""

from __future__ import annotations
from .errors import HTTP_ERROR_TYPES, ErrorResponse, error_response
from .session import (
    BranchesResponse,
    CompactRequest,
    CompactResponse,
    ContextStatsInfo,
    CreateSessionRequest,
    CreateSessionResponse,
    ForkSessionRequest,
    ForkSessionResponse,
    InterruptRequest,
    OkResponse,
    ReleaseRequest,
    ReleaseResponse,
    ResumeSessionRequest,
    ResumeSessionResponse,
    RewindRequest,
    RewindResponse,
    SendMessageRequest,
    SendMessageResponse,
    SessionGetResponse,
    SessionInfoResponse,
    SessionListResponse,
    SubscribeRequest,
    TagSessionRequest,
    TagSessionResponse,
    UnsubscribeRequest,
    UpdateSessionRequest,
    UpdateSessionResponse,
)
from .system import (
    AgentsResponse,
    CommandsResponse,
    HealthResponse,
    ModelCapabilities,
    ModelDetail,
    ModelsResponse,
    ProviderModels,
    RegisterToolsRequest,
    RegisterToolsResponse,
    ReloadResponse,
    ReloadResultItem,
    RemoteToolSpec,
    ToolInfo,
    ToolsListResponse,
)
from .ws import ClientRequest, ConnectResponse, ToolCallRequest, ToolCallResult


__all__ = [
    "AgentsResponse",
    "BranchesResponse",
    "ClientRequest",
    "CommandsResponse",
    "CompactRequest",
    "CompactResponse",
    "ConnectResponse",
    "ContextStatsInfo",
    "CreateSessionRequest",
    "CreateSessionResponse",
    "ErrorResponse",
    "ForkSessionRequest",
    "ForkSessionResponse",
    "HTTP_ERROR_TYPES",
    "HealthResponse",
    "InterruptRequest",
    "ModelCapabilities",
    "ModelDetail",
    "ModelsResponse",
    "OkResponse",
    "ProviderModels",
    "RegisterToolsRequest",
    "RegisterToolsResponse",
    "ReleaseRequest",
    "ReleaseResponse",
    "ReloadResponse",
    "ReloadResultItem",
    "RemoteToolSpec",
    "ResumeSessionRequest",
    "ResumeSessionResponse",
    "RewindRequest",
    "RewindResponse",
    "SendMessageRequest",
    "SendMessageResponse",
    "SessionGetResponse",
    "SessionInfoResponse",
    "SessionListResponse",
    "SubscribeRequest",
    "TagSessionRequest",
    "TagSessionResponse",
    "ToolCallRequest",
    "ToolCallResult",
    "ToolInfo",
    "ToolsListResponse",
    "UnsubscribeRequest",
    "UpdateSessionRequest",
    "UpdateSessionResponse",
    "error_response",
]
