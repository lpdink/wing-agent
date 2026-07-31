# wing/schema.py
import json
from typing import Any, Callable, Dict, List, Literal, Union

from pydantic import BaseModel, ConfigDict, Field, field_validator


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


#####


class ToolCall(BaseModel):
    id: str
    name: str
    arguments: dict

    def to_openai(self) -> dict:
        """Convert to OpenAI tool_calls format."""
        return {
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": json.dumps(self.arguments, ensure_ascii=False),
            },
        }

    def __repr__(self) -> str:
        args = json.dumps(self.arguments, ensure_ascii=False)
        return f"ToolCall({self.name}({args}))"


class LLMUsage(BaseModel):
    prompt_tokens: int = 0
    completion_tokens: int = 0
    cached_tokens: int = 0
    """prompt 中被缓存命中的 token 数"""
    first_chunk_rt_ms: float = 0.0
    """首包RT（毫秒）"""
    tokens_per_sec: float = 0.0
    """流式输出 tokens/s（基于服务端返回的 completion_tokens 精准计算）"""
    model: str = ""
    """模型名称，由 provider 在构建 usage 时注入"""
    request_id: str = ""
    """LLM API 响应的 x-request-id，用于排查问题"""

    def __repr__(self) -> str:
        parts = [f"in:{self.prompt_tokens} out:{self.completion_tokens}"]
        if self.model:
            parts.insert(0, f"model:{self.model}")
        if self.cached_tokens:
            parts.append(f"cached:{self.cached_tokens}")
        if self.first_chunk_rt_ms:
            parts.append(f"rt:{self.first_chunk_rt_ms:.0f}ms")
        if self.tokens_per_sec:
            parts.append(f"speed:{self.tokens_per_sec:.1f}t/s")
        return " ".join(parts)


class ToolCallDelta(BaseModel):
    """Incremental raw args fragment emitted during streaming.

    Carries only the args text accumulated since the last delta for this
    call (the first delta carries the full prefix). Clients accumulate
    fragments and partial-parse locally — the runtime never parses
    partial args.
    """

    id: str
    name: str
    args_fragment: str = ""
    is_final: bool = False


class LLMResponse(BaseModel):
    content: str | None = None
    reasoning_content: str | None = None
    reasoning_signature: str | None = None
    """Anthropic thinking block signature（多轮回放必需）。"""
    tool_calls: list[ToolCall] | None = None
    tool_call_deltas: list[ToolCallDelta] | None = None
    usage: LLMUsage = LLMUsage()


class ChainNode(BaseModel):
    """链节点基类，TrackedList 依赖的三个字段。"""

    uuid: str | None = None
    parent_uuid: str | None = None
    unzip_last_uuid: str | None = None


class Message(ChainNode):
    role: Literal["system", "user", "assistant", "tool"]
    content: str | None = None
    reasoning_content: str | None = None
    reasoning_signature: str | None = None
    """Anthropic thinking block signature（多轮回放必需）。"""
    tool_calls: list[ToolCall] | None = None  # assistant calls tools
    tool_call_id: str | None = None  # tool response only
    usage: "LLMUsage | None" = (
        None  # assistant 消息的 token 审计信息，持久化后重放可恢复
    )

    model_config = ConfigDict(extra="ignore", populate_by_name=True)

    def to_openai(self) -> dict:
        result: dict[str, Any] = {"role": self.role}
        result["content"] = self.content

        # reasoning content可能为""，此时要回传给llm
        if self.reasoning_content is not None:
            result["reasoning_content"] = self.reasoning_content

        if self.tool_calls:
            result["tool_calls"] = [tc.to_openai() for tc in self.tool_calls]

        if self.tool_call_id:
            result["tool_call_id"] = self.tool_call_id

        return result

    def __repr__(self) -> str:
        parts: list[str] = [self.role]
        if self.tool_call_id:
            parts.append(f"tool_call_id={self.tool_call_id}")
        if self.reasoning_content:
            parts.append(f"reasoning={self.reasoning_content!r}")
        if self.content:
            parts.append(f"content={self.content!r}")
        if self.tool_calls:
            # 显示完整的工具调用参数
            tc_strs = []
            for tc in self.tool_calls:
                args = json.dumps(tc.arguments, ensure_ascii=False)
                tc_strs.append(f"{tc.name}({args})")
            parts.append(f"tool_calls=[{', '.join(tc_strs)}]")
        return f"Message({' '.join(parts)})"

    def estimate_tokens(self) -> int:
        """Estimate token count from string representation.

        Uses TokenCounter as fallback——prefer server-side usage.prompt_tokens
        when available (see ContextManager._last_prompt_tokens).
        """
        from wing.common.token_counter import TokenCounter

        return TokenCounter.estimate_message(self)


# LLMUsage 在 Message 之后定义，Message.usage 引用它需要前向声明
# Pydantic 会通过 model_rebuild 在类定义完成后解析


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


class PendingCall(BaseModel):
    id: str = ""
    name: str = ""
    args_buffer: str = ""
    emitted_len: int = 0
    """args_buffer 中已作为流式碎片 emit 的长度（增量游标）。"""
