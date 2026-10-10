# wing/session/override.py
"""AgentOverride — 会话创建时的 agent 参数覆盖（领域类型）。

从 gateway/protocol.py 迁来的领域类型：Session.apply_agent_override 是唯一
应用点，HTTP 层只把它当请求体字段的承载。OpenAPI schema 与 Rust client
（crates/wing-api-client）依赖同一份形状。
"""

from __future__ import annotations

from typing import get_args

from pydantic import BaseModel, Field

from wing.common.utils import is_utf8_encodable


class AgentOverride(BaseModel):
    """Agent 参数覆盖。所有字段可选，None 表示不覆盖（保留 template 值）。

    ``model_id`` 是唯一的模型覆盖入口（引用 providers[].models 的 id）：命中即
    切到该 id 的调用名与 provider，未命中 raise（错误信息含 available ids）。
    provider 不再是覆盖字段（运行期事实，随映射而来）；旧字段 ``model`` /
    ``provider`` 由 pydantic ``extra="ignore"`` 静默忽略。
    """

    model_id: str | None = Field(
        default=None, description="覆盖模型（引用 providers[].models 的 id）"
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


def _text_field_names() -> tuple[str, ...]:
    """从模型定义派生"字符串字段"（新增字段自动纳入闸门，不再手写清单）。

    ``list[str]`` 字段（``tools``）**不**在此列：它的元素要逐个查（错误信息要能
    指到 ``tools[i]``），由 :func:`non_utf8_override_fields` 单独一遍。
    """
    names: list[str] = []
    for name, field in AgentOverride.model_fields.items():
        if str in (field.annotation, *get_args(field.annotation)):
            names.append(name)
    return tuple(names)


#: 覆盖里的字符串字段（可编码性闸门的覆盖面，按模型定义派生）。
_TEXT_FIELDS = _text_field_names()


def non_utf8_override_fields(override: AgentOverride) -> list[str]:
    """返回覆盖里**无法编码为 UTF-8** 的字段名（孤立代理字符等）；纯函数。

    与会话 id 闸门同口径（"合法 JSON ≠ 合法 UTF-8"）：这些字段全部会进
    metadata 落盘或 LLM 请求体，放行的后果是 500 + 半份 metadata——下一次同 id
    的 create 会"收养"这个半成品幽灵会话。
    """
    offending: list[str] = []
    for name in _TEXT_FIELDS:
        value = getattr(override, name)
        if isinstance(value, str) and not is_utf8_encodable(value):
            offending.append(name)
    for index, ref in enumerate(override.tools or []):
        if not is_utf8_encodable(ref):
            offending.append(f"tools[{index}]")
    return offending


def validate_override_utf8(override: AgentOverride) -> None:
    """覆盖的字符串字段必须可编码为 UTF-8，否则 ValueError（**应用之前**调用）。

    Raises:
        ValueError: 列出不可编码的字段名（不回声原始值——它本身就是不可编码的）。
    """
    offending = non_utf8_override_fields(override)
    if offending:
        raise ValueError(
            "agent override must be UTF-8 encodable: "
            f"{', '.join(offending)} (lone surrogates are not valid UTF-8)"
        )
