# wing/agent/event_sink.py
"""AgentEventSink — agent 包内事件发射的唯一出口。

所有 react loop 期间的事件（流式文本、工具调用、turn 结果等）均通过
AgentEventSink 的方法发射。内部自动注入 session_id 和 EventTarget，
调用方无需关心路由细节。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from wing.event import EventTarget
from wing.event_bus import event_bus
from wing.event import (
    AssistantTurnEvent,
    ContextStatsEvent,
    DoneEvent,
    ErrorEvent,
    LLMCallMetricsEvent,
    ReasoningEvent,
    TextEvent,
    ToolCallEvent,
    ToolCallResultEvent,
    ToolCallStreamEvent,
    ToolResultTurnEvent,
    TurnResultEvent,
    TurnStartedEvent,
)

if TYPE_CHECKING:
    from wing.event import WingEvent
    from wing.schema import LLMUsage, Message, ToolCall


class AgentEventSink:
    """事件发射唯一出口——构造时绑定 session_id。"""

    def __init__(self, session_id: str) -> None:
        self._session_id = session_id

    def _emit(self, event: WingEvent) -> None:
        if event.session_id is None:
            event.session_id = self._session_id
        if event.target is None:
            event.target = EventTarget(scope="session")
        event_bus.emit(event)

    # ── Turn 生命周期 ──

    def turn_started(self) -> None:
        self._emit(TurnStartedEvent(session_id=self._session_id))

    def turn_result(
        self,
        *,
        subtype: str = "success",
        result: str | None = None,
        num_turns: int = 0,
        duration_ms: int = 0,
        usage: dict | None = None,
        errors: list[str] | None = None,
        is_error: bool = False,
    ) -> None:
        self._emit(
            TurnResultEvent(
                session_id=self._session_id,
                subtype=subtype,
                is_error=is_error,
                result=result,
                num_turns=num_turns,
                duration_ms=duration_ms,
                usage=usage,
                errors=errors or [],
            )
        )

    def done(self) -> None:
        self._emit(DoneEvent(session_id=self._session_id))

    def error(self, message: str) -> None:
        self._emit(ErrorEvent(session_id=self._session_id, message=message))

    # ── Assistant turn ──

    def assistant_turn(self, msg: Message, model: str) -> None:
        self._emit(AssistantTurnEvent.from_message(msg, model, self._session_id))

    # ── Context stats ──

    def context_stats(
        self, message_count: int, total_tokens: int, context_window_tokens: int
    ) -> None:
        self._emit(
            ContextStatsEvent(
                session_id=self._session_id,
                message_count=message_count,
                total_tokens=total_tokens,
                context_window_tokens=context_window_tokens,
            )
        )

    # ── Tool events ──

    def tool_started(self, tc: ToolCall) -> None:
        self._emit(ToolCallEvent.from_tool_call(tc, self._session_id))

    def tool_finished(
        self, tc: ToolCall, result: str, *, success: bool, model: str
    ) -> None:
        """一次调用发射 ToolCallResultEvent + ToolResultTurnEvent。"""
        self._emit(
            ToolCallResultEvent.from_execution(
                tc, result, success, model, self._session_id
            )
        )
        self._emit(
            ToolResultTurnEvent.from_execution(tc, result, success, self._session_id)
        )

    # ── LLM streaming events ──

    def llm_text(self, content: str) -> None:
        self._emit(TextEvent(session_id=self._session_id, content=content))

    def llm_reasoning(self, content: str) -> None:
        self._emit(ReasoningEvent(session_id=self._session_id, content=content))

    def llm_tool_call_delta(
        self, tool_call_id: str, tool_name: str, args_fragment: str, is_final: bool
    ) -> None:
        self._emit(
            ToolCallStreamEvent(
                session_id=self._session_id,
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                args_fragment=args_fragment,
                is_final=is_final,
            )
        )

    def llm_metrics(self, usage: LLMUsage) -> None:
        self._emit(
            LLMCallMetricsEvent(
                session_id=self._session_id,
                prompt_tokens=usage.prompt_tokens,
                completion_tokens=usage.completion_tokens,
                cached_tokens=usage.cached_tokens,
                first_chunk_rt_ms=usage.first_chunk_rt_ms,
                tokens_per_sec=usage.tokens_per_sec,
                model=usage.model,
            )
        )
