# wing/schema/tool.py
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Literal, Union

from pydantic import BaseModel, ConfigDict, Field, field_validator

from .message import MediaRef


########## EXCEPTIONS
class ToolError(Exception):
    """Raised by tools to signal a non-happy-path result.

    Unlike generic exceptions, ToolError messages are passed directly
    to the model as tool results without an 'Error executing tool' prefix.
    """

    pass


########## SKILLS
class AgentSkill(BaseModel):
    """
    The agent skill typed dict class.
    reference from AgentScope
    """

    name: str
    """The name of the skill."""
    description: str
    """The description of the skill."""
    dir: str
    """The directory of the agent skill."""


@dataclass
class ToolOutput:
    """工具结构化结果：文本信封 + 媒体引用列表。

    工具返回值 `str` 仍完全兼容（存量工具零改动）。返回 `ToolOutput` 时
    ToolExecutor 把 media 原样带进 Message.media——截断与 `after_tool_call`
    hook 只作用于 content 文本（hook 拿不到 media，也无法改动它）。
    """

    content: str
    media: list[MediaRef] = field(default_factory=list)


class ToolParam(BaseModel):
    name: str
    type: Literal["string", "integer", "number", "boolean", "array", "object"]
    description: str = ""
    default: Any = None
    enum: Union[List[str], None] = None
    items: (
        dict
        | Literal["string", "integer", "number", "boolean", "array", "object"]
        | None
    ) = None
    """数组元素类型（仅 type=array 时使用）。
    简单类型传字符串如 "string"；嵌套 object 传完整 OpenAI schema dict。"""


class Tool(BaseModel):
    name: str
    """注册名（registry key），在同一命名空间内唯一。"""
    namespace: str = "default"
    """工具命名空间。内置工具为 "default"，远程工具可用 client ID 等。"""
    llm_name: str | None = None
    """LLM 可见名。None 时退化为 name。用于 to_openai() 输出和 agent 调度。"""
    description: str
    params: list[ToolParam]
    function: Callable
    inject_agent_param: str | None = Field(default=None, exclude=True)

    model_config = ConfigDict(arbitrary_types_allowed=True)

    @field_validator("namespace")
    @classmethod
    def _namespace_not_empty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("namespace must not be empty")
        return v

    @field_validator("llm_name")
    @classmethod
    def _llm_name_not_empty(cls, v: str | None) -> str | None:
        if v is not None and not v.strip():
            raise ValueError("llm_name must not be empty when provided")
        return v

    @property
    def effective_llm_name(self) -> str:
        """LLM 实际看到的工具名。llm_name 未设置时退化为 name。"""
        return self.llm_name if self.llm_name is not None else self.name

    def to_openai(self) -> dict:
        properties: Dict[str, Any] = {}
        required: List[str] = []

        for p in self.params:
            prop: Dict[str, Any] = {"type": p.type}
            if p.description:
                prop["description"] = p.description
            if p.enum:
                prop["enum"] = list(p.enum)
            if p.default is not None:
                prop["default"] = p.default
            if p.type == "array" and p.items:
                prop["items"] = (
                    p.items if isinstance(p.items, dict) else {"type": p.items}
                )

            properties[p.name] = prop
            if p.default is None:
                required.append(p.name)

        return {
            "type": "function",
            "function": {
                "name": self.effective_llm_name,
                "description": self.description,
                "parameters": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                },
            },
        }
