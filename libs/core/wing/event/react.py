# wing/event/react.py — Agent 产出事件（react loop 期间）

"""
Agent react loop 期间产生的事件：文本、推理、工具调用、指标、交互、完成。
"""

from __future__ import annotations

from typing import Any, Literal

from pydantic import Field

from .base import WingEvent


class TextEvent(WingEvent):
    type: Literal["text"] = "text"
    content: str


class ReasoningEvent(WingEvent):
    type: Literal["reasoning"] = "reasoning"
    content: str


class ToolCallEvent(WingEvent):
    type: Literal["tool_call"] = "tool_call"
    tool_name: str
    tool_args: dict[str, Any]
    tool_call_id: str


class ToolCallResultEvent(WingEvent):
    type: Literal["tool_call_result"] = "tool_call_result"
    tool_name: str
    tool_args: dict[str, Any]
    tool_call_id: str
    tool_result: str
    tool_success: bool
    model: str = ""


class LLMCallMetricsEvent(WingEvent):
    type: Literal["llm_call_metrics"] = "llm_call_metrics"
    model: str = ""
    prompt_tokens: int
    completion_tokens: int
    cached_tokens: int
    first_chunk_rt_ms: float
    tokens_per_sec: float


class AskEvent(WingEvent):
    type: Literal["ask"] = "ask"
    question: str
    choices: list[str] = Field(default_factory=list)
    required: bool = False


class DoneEvent(WingEvent):
    type: Literal["done"] = "done"


class TurnStartedEvent(WingEvent):
    """Agent turn 开始处理。在 agent._process_single_message() 入口 emit。

    与 DeliveredEvent 的区别：Delivered 是 transport ack（消息到达后端），
    TurnStarted 是语义信号（agent 开始处理该消息）。魔术命令不触发此事件。
    """

    type: Literal["turn_started"] = "turn_started"


class DiffContentEvent(WingEvent):
    """工具产生的 diff 内容，前端据此渲染 DiffView。"""

    type: Literal["diff_content"] = "diff_content"
    path: str
    old_text: str | None = None  # None 表示新文件（全绿）
    new_text: str
