# wing/schema.py
import json
from dataclasses import dataclass, field
from typing import Annotated, Any, Callable, Dict, List, Literal, Union

from pydantic import (
    BaseModel,
    ConfigDict,
    Field,
    PrivateAttr,
    field_validator,
    model_serializer,
    model_validator,
)


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
    arguments_error: str | None = None
    """工具参数解析失败的错误描述（含原始参数文本）。

    模型吐出的 args JSON 非法（如尾逗号）时，provider 不抛异常，而是
    置 arguments={} 并在此记录现场。ToolExecutor 见它短路执行，把错误
    作为工具结果回灌给模型自纠，而非整轮重试丢弃。
    """

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
    stop_reason: str | None = None
    """终止原因（协议原值：end_turn / max_tokens / tool_use / stop / length…）。

    provider 在最终响应的 usage 上设置；react_loop 据此传导进
    Message.stop_reason 与 LLMCallMetricsEvent——截断审计与补提交语义
    的唯一事实源。中断路径由 runtime 合成 "interrupted"。"""

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


class TextBlock(BaseModel):
    """assistant content block：普通文本。"""

    type: Literal["text"] = "text"
    text: str


class ThinkingBlock(BaseModel):
    """assistant content block：thinking（extended thinking）。

    signature 是 per-block 的（每个 thinking 块自带），绝不存在"全局一个
    signature"。redacted_thinking 复用本块：redacted=True 时 signature 存放
    加密 payload，作为不透明黑盒原样回放，永不解析。
    """

    type: Literal["thinking"] = "thinking"
    thinking: str = ""
    signature: str | None = None
    """per-block 签名。None/空串表示该块产生于不下发签名的推理 provider
    或存量旧数据——回放时原样发空签名（AnthropicProvider 回放契约）。"""
    redacted: bool = False


class ToolUseBlock(BaseModel):
    """assistant content block：工具调用。"""

    type: Literal["tool_use"] = "tool_use"
    id: str
    name: str
    input: dict = Field(default_factory=dict)
    input_error: str | None = None
    """参数 JSON 解析失败的现场记录（与 ToolCall.arguments_error 对应）。
    出错时 input 为 {}，本字段承载错误详情 + 原始参数文本。"""


ContentBlock = Annotated[
    Union[TextBlock, ThinkingBlock, ToolUseBlock],
    Field(discriminator="type"),
]
"""assistant 消息的有序 content block（真相源）。顺序即数组下标。"""


def _blocks_from_flat(
    reasoning_content: str | None,
    content: str | None,
    tool_calls: list | None,
) -> list[TextBlock | ThinkingBlock | ToolUseBlock] | None:
    """存量映射：扁平字段 → 块数组（thinking 无 signature）。

    develop 基线 JSONL 只有 content / reasoning_content / tool_calls，无块
    数组、无 signature。加载时自然映射进块数组作为唯一存储——无签名 thinking
    回放时原样发空签名（见 AnthropicProvider 回放契约）。
    """
    blocks: list[TextBlock | ThinkingBlock | ToolUseBlock] = []
    if reasoning_content:
        blocks.append(ThinkingBlock(thinking=reasoning_content))
    if content:
        blocks.append(TextBlock(text=content))
    for tc in tool_calls or []:
        if isinstance(tc, ToolCall):
            blocks.append(ToolUseBlock(id=tc.id, name=tc.name, input=tc.arguments))
        else:
            blocks.append(
                ToolUseBlock(
                    id=tc["id"], name=tc["name"], input=tc.get("arguments", {})
                )
            )
    return blocks or None


class LLMResponse(BaseModel):
    content: str | None = None
    reasoning_content: str | None = None
    content_blocks: list[ContentBlock] | None = None
    """provider 产出的结构化块数组（权威）。流式结束时由 provider 在最终
    chunk 给出，ReActLoop 组装进 Message。"""
    tool_calls: list[ToolCall] | None = None
    tool_call_deltas: list[ToolCallDelta] | None = None
    usage: LLMUsage = LLMUsage()


class ChainNode(BaseModel):
    """链节点基类，TrackedList 依赖的三个字段。"""

    uuid: str | None = None
    parent_uuid: str | None = None
    unzip_last_uuid: str | None = None


class MediaRef(BaseModel):
    """图片媒体引用（内容寻址，仅元数据不含字节）。

    字节由 SessionStore 以 id 为键托管（file 后端 `<sessions-root>/.media/`，
    memory 后端进程内）。history.jsonl 只落本引用——绝不落 base64。

    注：id 的格式校验（64 位小写 hex，防路径穿越）在**存储层**，模型层保持
    宽松——一条损坏的历史记录不至于让整条 Message 加载失败（加载路径对未知/
    非法值应有容忍面，字节读取失败由消费方降级）。
    """

    id: str
    """sha256 hex（64 字符），即存储文件名。"""
    mime: str
    """image/png | image/jpeg | image/webp | image/gif"""
    bytes: int
    """原始字节数。"""
    width: int
    height: int
    name: str | None = None
    """展示名（basename，不含路径）。"""


class Message(ChainNode):
    role: Literal["system", "user", "assistant", "tool"]
    content_blocks: list[ContentBlock] | None = None
    """assistant 消息的有序 content block 数组——assistant 的唯一存储。
    顺序即数组下标。非 assistant 消息不携带。"""
    media: list[MediaRef] | None = None
    """消息携带的图片引用（仅引用，字节在 SessionStore）。

    当前仅 tool 消息使用（预留 user）；None = 无媒体——落盘时 None 被
    自然剥除，无媒体消息的 history 记录与引入 media 之前完全一致。"""
    tool_call_id: str | None = None  # tool response only
    usage: "LLMUsage | None" = (
        None  # assistant 消息的 token 审计信息，持久化后重放可恢复
    )
    stop_reason: str | None = None
    """assistant 消息的终止原因（正常 stop / max_tokens 截断 / interrupted 补提交）。

    审计元数据——不进入 LLM 请求体（to_openai 不导出）。extra="ignore"
    容忍存量记录缺失。"""

    # 扁平存储：仅非 assistant 消息（user/tool/system）使用。assistant 的
    # content / reasoning_content / tool_calls 全部从 content_blocks 实时派生，
    # 不存在第二份存储。
    _content: str | None = PrivateAttr(default=None)
    _tool_calls: list[ToolCall] | None = PrivateAttr(default=None)

    model_config = ConfigDict(extra="ignore", populate_by_name=True)

    @field_validator("media", mode="before")
    @classmethod
    def _empty_media_normalized_to_none(cls, v: Any) -> Any:
        """空列表归一为 None："无媒体"在内存与落盘只有一种表示。

        否则显式传入的空列表会以 ``"media": []`` 落进 history.jsonl——
        无信息的新键，还会让"无媒体记录与引入 media 前完全一致"不再成立。
        """
        if isinstance(v, (list, tuple)) and len(v) == 0:
            return None
        return v

    def __init__(
        self,
        *,
        role: Literal["system", "user", "assistant", "tool"],
        content: str | None = None,
        reasoning_content: str | None = None,
        content_blocks: list[ContentBlock] | None = None,
        media: list[MediaRef] | None = None,
        tool_calls: list[ToolCall] | None = None,
        tool_call_id: str | None = None,
        usage: LLMUsage | None = None,
        stop_reason: str | None = None,
        uuid: str | None = None,
        parent_uuid: str | None = None,
        unzip_last_uuid: str | None = None,
        **extra: Any,
    ) -> None:
        """显式签名供类型检查器识别扁平入参；路由逻辑在 `_route_flat`
        wrap validator（构造与 model_validate 加载共用）。**extra 透传
        未知键（如 MessageLog 附加的 ts），由 extra="ignore" 丢弃。"""
        data: dict[str, Any] = {
            "role": role,
            "content": content,
            "reasoning_content": reasoning_content,
            "content_blocks": content_blocks,
            "media": media,
            "tool_calls": tool_calls,
            "tool_call_id": tool_call_id,
            "usage": usage,
            "stop_reason": stop_reason,
            "uuid": uuid,
            "parent_uuid": parent_uuid,
            "unzip_last_uuid": unzip_last_uuid,
        }
        data.update(extra)
        super().__init__(**data)

    @model_validator(mode="wrap")
    @classmethod
    def _route_flat(cls, data: Any, handler: Callable[[Any], "Message"]) -> "Message":
        """单一存储路由：扁平入参（content / reasoning_content / tool_calls）
        不进字段，按 role 分流——

        - assistant：无 content_blocks 时由扁平值构建块数组（存量旧格式映射，
          thinking 无 signature）；有 content_blocks 时扁平值忽略（派生）。
        - 非 assistant：写入私有存储，property 直通。

        构造（__init__）与加载（model_validate）共用此路径，业务代码无
        `if 旧格式` 特判。
        """
        if not isinstance(data, dict):
            return handler(data)
        data = dict(data)
        content = data.pop("content", None)
        reasoning_content = data.pop("reasoning_content", None)
        tool_calls = data.pop("tool_calls", None)

        if data.get("role") == "assistant":
            if data.get("content_blocks") is None:
                data["content_blocks"] = _blocks_from_flat(
                    reasoning_content, content, tool_calls
                )
            msg = handler(data)
            return msg

        msg = handler(data)
        msg._content = content
        if tool_calls:
            msg._tool_calls = [
                tc if isinstance(tc, ToolCall) else ToolCall.model_validate(tc)
                for tc in tool_calls
            ]
        return msg

    @model_serializer(mode="wrap")
    def _serialize_flat(self, handler: Callable[[Any], dict]) -> dict:
        """导出派生的扁平字段（向后兼容旧消费方与落盘格式）。

        导出是派生行为，不是第二份存储：assistant 的 content /
        reasoning_content / tool_calls 实时取自 content_blocks。
        """
        data = handler(self)
        data["content"] = self.content
        data["reasoning_content"] = self.reasoning_content
        data["tool_calls"] = (
            [tc.model_dump() for tc in self.tool_calls] if self.tool_calls else None
        )
        return data

    @property
    def content(self) -> str | None:
        """assistant：拼接 TextBlock（实时派生）；非 assistant：存储值。"""
        if self.role == "assistant":
            if not self.content_blocks:
                return None
            parts = [b.text for b in self.content_blocks if isinstance(b, TextBlock)]
            return "".join(parts) or None
        return self._content

    @content.setter
    def content(self, value: str | None) -> None:
        if self.role == "assistant":
            raise AttributeError(
                "assistant message content is derived from content_blocks; "
                "mutate the blocks instead"
            )
        self._content = value

    @property
    def reasoning_content(self) -> str | None:
        """拼接非 redacted ThinkingBlock 的 thinking 文本（实时派生）。"""
        if not self.content_blocks:
            return None
        parts = [
            b.thinking
            for b in self.content_blocks
            if isinstance(b, ThinkingBlock) and not b.redacted and b.thinking
        ]
        return "".join(parts) or None

    @property
    def tool_calls(self) -> list[ToolCall] | None:
        """assistant：抽取 ToolUseBlock（实时派生）；非 assistant：存储值。"""
        if self.role == "assistant":
            if not self.content_blocks:
                return None
            calls = [
                ToolCall(
                    id=b.id,
                    name=b.name,
                    arguments=b.input,
                    arguments_error=b.input_error,
                )
                for b in self.content_blocks
                if isinstance(b, ToolUseBlock)
            ]
            return calls or None
        return self._tool_calls

    @tool_calls.setter
    def tool_calls(self, value: list[ToolCall] | None) -> None:
        if self.role == "assistant":
            raise AttributeError(
                "assistant message tool_calls is derived from content_blocks; "
                "mutate the blocks instead"
            )
        self._tool_calls = value

    def to_openai(self) -> dict:
        result: dict[str, Any] = {"role": self.role}
        # wire 契约：content 恒为非 null 字符串。assistant 在 thinking-only 轮
        # （打断补提交 / max_tokens 截断在 thinking 中途）派生 content 为
        # None，严格 OpenAI 兼容网关（阿里云 MaaS 等）对「无 tool_calls 且
        # content 为 null」直接 400。内部 / 磁盘 / 前端保留 None 语义，
        # 仅此处（唯一 wire 投影出口）兜底为空串。
        result["content"] = self.content if self.content is not None else ""

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


@dataclass
class ToolOutput:
    """工具结构化结果：文本信封 + 媒体引用列表。

    工具返回值 `str` 仍完全兼容（存量工具零改动）。返回 `ToolOutput` 时
    ToolExecutor 把 media 原样带进 Message.media——截断与 `after_tool_call`
    hook 只作用于 content 文本（hook 拿不到 media，也无法改动它）。
    """

    content: str
    media: list[MediaRef] = field(default_factory=list)


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
