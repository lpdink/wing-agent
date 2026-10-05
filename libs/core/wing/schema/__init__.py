# wing/schema/__init__.py
"""wing/schema 包 — 领域模型（消息 / LLM 协议类型 / 工具注册类型）。

公共 API 通过此 __init__ re-export（消费方 import 路径不变）：

    from wing.schema import Message, Tool, ToolParam, ToolError, MediaRef, ...

内部模块：``message``（ChainNode / 内容块 / MediaRef / Message——落盘格式守门人）·
``llm``（LLMUsage / LLMResponse / ToolCall / ToolCallDelta / PendingCall）·
``tool``（ToolError / ToolParam / Tool / ToolOutput / AgentSkill）。

注意：``message`` 与 ``llm`` 互引对方的类型（Message.usage ↔ LLMUsage、
Message 扁平 tool_calls ↔ ToolCall、LLMResponse.content_blocks ↔ ContentBlock）。
解环方式：两模块各自把对方的导入放在**文件底部**（类型只在 pydantic 构建时求值），
导入完成后各自 ``model_rebuild()``——本包内导入顺序不敏感。
"""

from .llm import LLMResponse, LLMUsage, PendingCall, ToolCall, ToolCallDelta
from .message import (
    ChainNode,
    ContentBlock,
    MediaRef,
    Message,
    TextBlock,
    ThinkingBlock,
    ToolUseBlock,
)
from .tool import AgentSkill, Tool, ToolError, ToolOutput, ToolParam

__all__ = [
    "AgentSkill",
    "ChainNode",
    "ContentBlock",
    "LLMResponse",
    "LLMUsage",
    "MediaRef",
    "Message",
    "PendingCall",
    "TextBlock",
    "ThinkingBlock",
    "Tool",
    "ToolCall",
    "ToolCallDelta",
    "ToolError",
    "ToolOutput",
    "ToolParam",
    "ToolUseBlock",
]
