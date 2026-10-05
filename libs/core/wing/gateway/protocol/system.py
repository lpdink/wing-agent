# wing/gateway/protocol/system.py — 系统面协议模型

"""系统面端点协议模型——非会话的网关端点。

系统查询（commands / models / agents）、热重载（reload）、工具列表与远程工具
注册、健康检查。字段与文案即线格式：改动等于改协议。
"""

from __future__ import annotations

import re

from pydantic import BaseModel, Field, field_validator, model_validator

from wing.event import CommandInfo
from wing.schema import ToolParam


# ============================================================
# 系统面响应（健康 / 重载 / 工具 / 命令 / 模型 / agent）
# ============================================================


class HealthResponse(BaseModel):
    """健康检查响应。"""

    service: str = Field(default="wing-gateway", description="服务身份标识")
    status: str = Field(default="ok", description="服务状态")
    version: str = Field(description="网关版本号（构建时注入）")
    commit: str | None = Field(
        default=None, description="构建时注入的 commit hash（短）；未知为 null"
    )
    uptime: int = Field(description="Gateway 运行时长（秒）")


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


class CommandsResponse(BaseModel):
    """GET /api/commands 响应——可用命令列表。"""

    commands: list[CommandInfo] = Field(
        default_factory=list, description="所有已注册的 magic command"
    )


class ModelCapabilities(BaseModel):
    """模型能力声明（GET /api/models 透出；未声明 = 全 false）。"""

    vision: bool = Field(default=False, description="是否接受图片输入")


class ModelDetail(BaseModel):
    """单条模型声明的详情（与同组 ``models`` 逐项同序对应）。"""

    name: str = Field(description="实际调用名")
    display_name: str | None = Field(
        default=None, description="展示名（可空，前端回落 name）"
    )
    description: str | None = Field(default=None, description="模型描述")
    capabilities: ModelCapabilities = Field(
        default_factory=ModelCapabilities, description="能力声明"
    )


class ProviderModels(BaseModel):
    """单个 provider 的可用模型（嵌套模型列表条目）。"""

    provider: str = Field(description="Provider 名称")
    models: list[str] = Field(
        default_factory=list, description="该 provider 的模型名列表"
    )
    model_details: list[ModelDetail] = Field(
        default_factory=list,
        description="模型声明详情；与 models 逐项同序同名（追加属性，可为空）",
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


# ============================================================
# 远程工具注册（tool host 经 HTTP 注册，调用帧走 WS）
# ============================================================


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


class RegisterToolsResponse(BaseModel):
    """注册远程工具的响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    registered: list[str] = Field(
        default_factory=list,
        description="已注册工具的完整引用列表（形如 <client_id>.<name>）",
    )
