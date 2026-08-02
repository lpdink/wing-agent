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

import re
import uuid
from collections.abc import Mapping
from typing import Literal

from pydantic import BaseModel, Field, field_validator, model_validator
from starlette.responses import JSONResponse

from wing.event import AgentInfo, CommandInfo, SessionInfo
from wing.event.query_response import BranchTargetInfo
from wing.schema import ToolParam


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
        default=None, description="Reasoning effort: low|medium|high|xhigh|max"
    )
    yolo: bool | None = Field(
        default=None, description="跳过危险命令审查（None 表示不覆盖）"
    )


class CreateSessionRequest(BaseModel):
    """创建新 session 的请求体。"""

    template_name: str | None = Field(
        default=None, description="Agent 模板名称，None 使用默认模板"
    )
    workspace: str | None = Field(default=None, description="工作目录路径")
    agent: AgentOverride | None = Field(default=None, description="Agent 参数覆盖")
    backend: str | None = Field(
        default=None,
        description=(
            "存储后端：file（默认，落盘）| memory（session 状态不落盘，仅本次进程有效；"
            "注意 metrics 审计文件不受此约束）"
        ),
    )


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
    tool_call_id: str | None = Field(
        default=None,
        description="回复某个 Ask 事件时携带其 tool_call_id，定向 resolve feedback waiter",
    )


class CompactRequest(BaseModel):
    """压缩 session 上下文的请求体。"""

    session_id: str = Field(description="目标 session ID")


class InterruptRequest(BaseModel):
    """中断 session 当前任务的请求体。"""

    session_id: str = Field(description="目标 session ID")


class RewindRequest(BaseModel):
    """回退 session 到指定消息节点的请求体。"""

    session_id: str = Field(description="目标 session ID")
    target_uuid: str = Field(description="要回退到的消息 UUID")


# LLM 可见工具名须符合 provider function-name 文法（OpenAI: ^[a-zA-Z0-9_-]{1,64}$）。
# 远程工具是首个真正使用自定义 llm_name 的消费方，注册时即校验，避免坏名字
# 延迟到 LLM 调用时才 confusing 地失败。
_LLM_NAME_RE = re.compile(r"^[a-zA-Z0-9_-]{1,64}$")


class RemoteToolSpec(BaseModel):
    """远程工具规格——tool host 注册单个工具的 schema。

    复用核心 ToolParam，与内置工具 schema 同构。注册后以 client_id 为
    namespace 落入核心 registry，配置引用形如 ``<client_id>.<name>``。

    llm_name 由端上自选（LLM 实际看到的名字）；None 时退化为裸 name。
    运行时**不**自动以 client_id 限定 llm_name——当前模型尚不适合同时持有
    两个同名工具（如两个 Read），两个 effective_llm_name 相同的工具绑定到
    同一 agent 会在 create session 时失败，这是预期行为。
    """

    name: str = Field(description="工具注册名（同 namespace 内唯一）")
    description: str = Field(default="", description="工具描述（LLM 可见）")
    llm_name: str | None = Field(
        default=None, description="LLM 可见名（None 退化为 name；须符合 provider 文法）"
    )
    params: list[ToolParam] = Field(
        default_factory=list, description="工具参数列表（复用核心 ToolParam）"
    )

    @field_validator("name")
    @classmethod
    def _name_resolvable(cls, v: str) -> str:
        # 工具名含 "." 会破坏 ToolRef.parse（rsplit(".", 1)）：注册成功却永远
        # 无法以 "<client_id>.<name>" 解析——静默坑，注册时即拒绝。
        if not v.strip():
            raise ValueError("tool name must not be empty")
        if v != v.strip():
            raise ValueError("tool name must not have leading/trailing whitespace")
        if "." in v:
            raise ValueError("tool name must not contain '.'")
        return v

    @field_validator("llm_name")
    @classmethod
    def _llm_name_provider_safe(cls, v: str | None) -> str | None:
        if v is None:
            return v
        if not _LLM_NAME_RE.match(v):
            raise ValueError(
                "llm_name must match ^[a-zA-Z0-9_-]{1,64}$ (provider function-name grammar)"
            )
        return v

    @model_validator(mode="after")
    def _effective_llm_name_provider_safe(self) -> "RemoteToolSpec":
        # effective LLM 可见名（llm_name or name）须过 provider 文法——否则
        # llm_name=None 时裸 name（如 "My Tool" / "读文件"）会注册通过、却延迟到
        # LLM 调用才 400，违反"坏名字应在注册时失败"。即使将来 llm_name 按存废
        # 判断被删、name 成为唯一 LLM 可见名，这道校验依然必要。
        effective = self.llm_name or self.name
        if not _LLM_NAME_RE.match(effective):
            raise ValueError(
                "effective LLM-visible name (llm_name or name) must match "
                "^[a-zA-Z0-9_-]{1,64}$ (provider function-name grammar)"
            )
        return self


class RegisterToolsRequest(BaseModel):
    """注册远程工具的请求体。需要 X-Client-Id header 标识 tool host。"""

    tools: list[RemoteToolSpec] = Field(description="要注册的工具规格列表")


# ============================================================
# HTTP Response Models
# ============================================================


class CreateSessionResponse(BaseModel):
    """创建 session 的响应。"""

    session_id: str = Field(description="新创建的 session ID")
    template_name: str = Field(description="使用的模板名称")
    workspace: str | None = Field(default=None, description="工作目录路径")
    backend: str = Field(default="file", description="存储后端（file/memory）")


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


class RegisterToolsResponse(BaseModel):
    """注册远程工具的响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    registered: list[str] = Field(
        default_factory=list,
        description="已注册工具的完整引用列表（形如 <client_id>.<name>）",
    )


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
    status: str = Field(
        default="idle", description="运行时状态: inactive|idle|working|waiting"
    )
    messages: list[dict] = Field(description="消息历史列表")
    agent: AgentInfo | None = Field(default=None, description="当前 agent 配置信息")


class HealthResponse(BaseModel):
    """健康检查响应。"""

    service: str = Field(default="wing-gateway", description="服务身份标识")
    status: str = Field(default="ok", description="服务状态")
    version: str = Field(description="wing-agent 版本号")
    uptime: int = Field(description="Gateway 运行时长（秒）")


class ErrorResponse(BaseModel):
    """错误响应。"""

    error: str = Field(description="错误类型")
    detail: str | None = Field(default=None, description="错误详细信息")
    session_id: str | None = Field(default=None, description="关联的 session ID")
    uuid: str | None = Field(default=None, description="关联的消息 UUID")


# ErrorResponse 是前后端错误形状的唯一约定，error_response() 是它的唯一序列化
# 出口——gateway 的 exception handler 与鉴权中间件共用，保证路由异常、请求校验
# 失败、鉴权拒绝都输出同一形状，wing-api-client 因此总能结构化解析。
# 仅收录实际会被产生的状态码，避免出现误导性的死映射项。
HTTP_ERROR_TYPES: dict[int, str] = {
    400: "bad_request",
    401: "unauthorized",
    403: "forbidden",
    404: "not_found",
    405: "method_not_allowed",
    422: "validation_error",
    500: "internal_error",
    504: "gateway_timeout",
}


def error_response(
    status_code: int,
    detail: str | None = None,
    *,
    error: str | None = None,
    headers: Mapping[str, str] | None = None,
) -> JSONResponse:
    """构造统一形状（ErrorResponse）的错误响应。"""
    body = ErrorResponse(
        error=error or HTTP_ERROR_TYPES.get(status_code, "error"),
        detail=detail,
    )
    return JSONResponse(
        status_code=status_code,
        content=body.model_dump(exclude_none=True),
        headers=headers,
    )


# ============================================================
# Session 查询端点 Response
# ============================================================


class ContextStatsInfo(BaseModel):
    """上下文统计信息，嵌入 SessionInfoResponse。"""

    message_count: int = Field(description="当前消息数量")
    total_tokens: int = Field(description="当前上下文 token 总数")


class SessionInfoResponse(BaseModel):
    """GET /api/session/info 响应——session 运行时状态。"""

    model: str = Field(description="当前模型名称")
    api_url: str = Field(description="API 基础 URL")
    tools: list[str] = Field(description="已启用的工具名称列表")
    total_tokens: int = Field(description="当前上下文 token 总数")
    context_window_tokens: int = Field(description="上下文窗口大小")
    thinking: bool = Field(description="thinking 模式是否开启")
    reasoning_effort: str | None = Field(
        default=None, description="推理力度: low|medium|high|xhigh|max"
    )
    yolo: bool = Field(description="yolo 模式是否开启")
    session_name: str | None = Field(default=None, description="session 名称")
    workdir: str | None = Field(
        default=None,
        description="session 工作目录（session workspace，非进程启动目录）",
    )
    status: str = Field(
        default="idle", description="运行时状态: inactive|idle|working|waiting"
    )
    context_stats: ContextStatsInfo = Field(description="上下文统计信息")
    skills_info: str = Field(default="", description="已安装的 skills 信息")
    system_prompt: str = Field(default="", description="完整系统提示词")


class CompactResponse(BaseModel):
    """POST /api/session/compact 响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    original_tokens: int = Field(default=0, description="压缩前 token 数")
    compressed_tokens: int = Field(default=0, description="压缩后 token 数")


class RewindResponse(BaseModel):
    """POST /api/session/rewind 响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    draft: str | None = Field(default=None, description="回退点处的用户消息草稿")


class ReloadResultItem(BaseModel):
    """reload 端点中每一项重载的结果。"""

    name: str = Field(description="重载项名称")
    ok: bool = Field(description="是否成功")
    detail: str | None = Field(default=None, description="失败时的错误详情")


class ReloadResponse(BaseModel):
    """POST /api/system/reload 响应。"""

    ok: bool = Field(description="全部重载是否成功")
    results: list[ReloadResultItem] = Field(
        default_factory=list, description="每项重载的结果"
    )


class BranchesResponse(BaseModel):
    """GET /api/session/branches 响应——可回退/分叉的消息节点列表。"""

    targets: list[BranchTargetInfo] = Field(
        default_factory=list, description="可回退/分叉的消息节点"
    )


# ============================================================
# Session 更新端点 Request / Response
# ============================================================


class UpdateSessionRequest(BaseModel):
    """POST /api/session/update 请求——统一 session 状态变更。"""

    session_id: str = Field(description="目标 session ID")
    model: str | None = Field(default=None, description="切换模型")
    provider: str | None = Field(
        default=None, description="切换 provider（配合 model 使用）"
    )
    agent: str | None = Field(default=None, description="切换 agent 模板")
    title: str | None = Field(default=None, description="设置 session 名称")
    thinking: bool | None = Field(default=None, description="开关 thinking 模式")
    reasoning_effort: str | None = Field(
        default=None, description="推理力度: low|medium|high|xhigh|max"
    )
    yolo: bool | None = Field(default=None, description="开关 yolo 模式")
    workspace: str | None = Field(default=None, description="切换工作目录路径")
    tools: list[str] | None = Field(
        default=None,
        description="切换工具集（全量替换，ref 格式：namespace.name 或裸名）",
    )


class UpdateSessionResponse(BaseModel):
    """POST /api/session/update 响应。"""

    ok: bool = Field(default=True, description="操作是否成功")


class ToolInfo(BaseModel):
    """单个工具的元信息。"""

    ref: str = Field(description="工具引用（namespace.name 或裸名）")
    namespace: str = Field(description="工具命名空间")
    name: str = Field(description="注册名")
    llm_name: str = Field(description="LLM 可见名（effective_llm_name）")
    description: str = Field(default="", description="工具描述")


class ToolsListResponse(BaseModel):
    """GET /api/tools 响应——全局工具列表。"""

    tools: list[ToolInfo] = Field(default_factory=list, description="所有已注册工具")


# ============================================================
# 系统级查询端点 Response
# ============================================================


class CommandsResponse(BaseModel):
    """GET /api/commands 响应——可用命令列表。"""

    commands: list[CommandInfo] = Field(
        default_factory=list, description="所有已注册的 magic command"
    )


class ProviderModels(BaseModel):
    """单个 provider 的可用模型（嵌套模型列表条目）。"""

    provider: str = Field(description="Provider 名称")
    models: list[str] = Field(
        default_factory=list, description="该 provider 的模型名列表"
    )


class ModelsResponse(BaseModel):
    """GET /api/models 响应——可用模型列表（按 provider 分组嵌套）。"""

    providers: list[ProviderModels] = Field(
        default_factory=list, description="按 provider 分组的模型列表"
    )


class AgentsResponse(BaseModel):
    """GET /api/agents 响应——可用 agent 模板列表。"""

    agents: list[str] = Field(
        default_factory=list, description="所有可用 agent 模板名称"
    )
    default_agent: str = Field(description="默认 agent 模板名称")
