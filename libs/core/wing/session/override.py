# wing/session/override.py
"""AgentOverride — 会话创建时的 agent 参数覆盖（领域类型）。

从 gateway/protocol.py 迁来的领域类型：Session.apply_agent_override 是唯一
应用点，HTTP 层只把它当请求体字段的承载。字段逐字不变——OpenAPI schema 与
Rust client（crates/wing-api-client）依赖同一份形状。
"""

from __future__ import annotations

from pydantic import BaseModel, Field


class AgentOverride(BaseModel):
    """Agent 参数覆盖。所有字段可选，None 表示不覆盖（保留 template 值）。

    provider 与 model 配合使用：指定 provider 时，model 切换到该 provider
    的 endpoint；不指定时使用当前 provider。
    """

    model: str | None = Field(default=None, description="覆盖模型名称")
    provider: str | None = Field(
        default=None,
        description="覆盖 provider 名称（配合 model 使用，引用 providers[].name）",
    )
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
