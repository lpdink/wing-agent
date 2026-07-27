# wing/event/react.py — Agent 产出事件（react loop 期间）

"""
Agent react loop 期间产生的事件：文本、推理、工具调用、指标、交互、完成。
"""

from __future__ import annotations

import uuid as _uuid
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


class ToolCallStreamEvent(WingEvent):
    """Streaming tool call args fragment — emitted during LLM generation.

    Carries the incremental raw args text accumulated since the last event
    for this call (the first event carries the full prefix so far). Clients
    accumulate fragments and partial-parse locally for real-time rendering;
    the runtime forwards provider text as-is and never parses partial args.
    Coexists with ToolCallEvent: this fires during arg generation,
    ToolCallEvent fires when execution begins (authoritative parsed args).
    """

    type: Literal["tool_call_stream"] = "tool_call_stream"
    tool_call_id: str
    tool_name: str
    args_fragment: str = ""
    is_final: bool = False


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
    # 关联的工具调用 id。客户端回复时经 post(tool_call_id=...) 定向 resolve
    # 对应的 feedback waiter（并发 ask 场景下区分回复归属）。
    tool_call_id: str = ""
    # Multi-question format (AskUserQuestion tool)
    questions: list[dict] = Field(default_factory=list)
    # Legacy single-question format (Bash dangerous command confirmation)
    question: str = ""
    choices: list[str] = Field(default_factory=list)
    required: bool = False


class DoneEvent(WingEvent):
    type: Literal["done"] = "done"


class TurnStartedEvent(WingEvent):
    """Agent turn 开始处理。在 agent._process_turn() 入口 emit。

    与 DeliveredEvent 的区别：Delivered 是 transport ack（消息到达后端），
    TurnStarted 是语义信号（agent 开始处理该消息）。魔术命令不触发此事件。
    """

    type: Literal["turn_started"] = "turn_started"


class DiffContentEvent(WingEvent):
    """工具产生的 diff 内容，前端据此渲染 DiffView。

    tool_call_id 关联产生此 diff 的工具调用（Write/Edit/BetterEdit）。
    并发工具调用场景下事件乱序到达，前端据此把 diff 锚定到对应
    ToolCall cell 之后，而非追加到聊天尾部。取自 agent.py 的
    current_tool_call_id()（exec_tool_calls 为每个 gather task 设置）。
    """

    type: Literal["diff_content"] = "diff_content"
    path: str
    old_text: str | None = None  # None 表示新文件（全绿）
    new_text: str
    tool_call_id: str = ""


# ── Turn-level events (for stdio / SDK consumers) ──────────────────────
# These coexist with the streaming events above. TUI ignores them;
# stdio frontends consume them to produce Claude-compatible NDJSON output.


class AssistantTurnEvent(WingEvent):
    """Turn 级别的 assistant 完整消息（对应 Claude SDKAssistantMessage）。

    在 _call_llm() 返回完整 assistant 消息后、exec_tool_calls() 之前 emit。
    content_blocks 格式:
      [{"type": "text", "text": "..."},
       {"type": "tool_use", "id": "call_xxx", "name": "Bash", "input": {...}},
       {"type": "thinking", "thinking": "..."}]
    """

    type: Literal["assistant_turn"] = "assistant_turn"
    uuid: str = Field(default_factory=lambda: _uuid.uuid4().hex)
    content_blocks: list[dict]
    model: str = ""
    stop_reason: str | None = None
    usage: dict | None = None


class ToolResultTurnEvent(WingEvent):
    """Turn 级别的 tool result（对应 Claude SDKUserMessage tool_result block）。

    在每个工具执行完成后 emit，伴随现有的 ToolCallResultEvent。
    """

    type: Literal["tool_result_turn"] = "tool_result_turn"
    uuid: str = Field(default_factory=lambda: _uuid.uuid4().hex)
    tool_use_id: str
    tool_name: str
    content: str
    is_error: bool = False


class TurnResultEvent(WingEvent):
    """整个 agent loop 的最终结果（对应 Claude SDKResultMessage）。

    在整个 agent loop 结束时 emit，位于 DoneEvent 之前。
    subtype: "success" | "error_during_execution" | "error_max_turns"
    """

    type: Literal["turn_result"] = "turn_result"
    uuid: str = Field(default_factory=lambda: _uuid.uuid4().hex)
    subtype: str = "success"
    is_error: bool = False
    result: str | None = None
    num_turns: int = 0
    duration_ms: int = 0
    usage: dict | None = None
    errors: list[str] = Field(default_factory=list)
