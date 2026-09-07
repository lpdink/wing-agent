# wing/agent/event_sink.py
"""AgentEventSink — agent 包内事件发射的唯一出口。

所有 react loop 期间的事件（流式文本、工具调用、turn 结果等）均通过
AgentEventSink 的方法发射。内部自动注入 session_id 和 EventTarget，
调用方无需关心路由细节。

持久化分流（两次 commit 语义）：
- persist=true → 经 append_event 落盘进混合链（即时，事件完整产生时刻）
  再广播——日志是事实源，广播是投影；
- persist=false → 记入 EventJournal（RAM 合成缓冲，供中途订阅者重放）
  再广播，绝不落盘；turn 收口时由 Message 记录承载其内容。

工具侧派生事件（DiffContentEvent 等经 ctx.emit / WingAgent.emit）同样
路由至此——sink 是唯一的发射出口，不存在绕过 sink 的直连 event_bus。
"""

from __future__ import annotations

from collections.abc import Callable
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
    UserMessageAcceptedEvent,
)

from .event_journal import EventJournal

if TYPE_CHECKING:
    from wing.event import WingEvent
    from wing.schema import LLMUsage, Message, ToolCall


class AgentEventSink:
    """事件发射唯一出口——构造时绑定 session_id 与事件落盘回调。"""

    def __init__(
        self,
        session_id: str,
        append_event: Callable[[WingEvent], None] | None = None,
    ) -> None:
        self._session_id = session_id
        # 事件落盘回调（ContextManager.append_event）——None 时纯内存
        # （无持久化语义的场景，如测试）。
        self._append_event = append_event
        # 当前 turn 的瞬态事件缓冲（RAM commit 层）
        self.journal = EventJournal()

    def _emit(self, event: WingEvent) -> None:
        if event.session_id is None:
            event.session_id = self._session_id
        if event.target is None:
            event.target = EventTarget(scope="session")

        # 分流：先事实（落盘/RAM），后投影（广播）
        if event.persist:
            if self._append_event is not None:
                self._append_event(event)
        else:
            self.journal.record(event)

        event_bus.emit(event)

    # ── Turn 生命周期 ──

    def turn_started(self) -> None:
        self._emit(TurnStartedEvent(session_id=self._session_id))

    def user_message_accepted(
        self, content: str, origin_request_id: str | None
    ) -> None:
        """用户消息被消费进模型上下文（新 turn 输入或 steer 注入）。"""
        self._emit(
            UserMessageAcceptedEvent(
                session_id=self._session_id,
                content=content,
                origin_request_id=origin_request_id or "",
            )
        )

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
                stop_reason=usage.stop_reason,
            )
        )

    # ── 瞬态缓冲控制 ──

    def clear_journal(self) -> None:
        """turn 收口后清空瞬态缓冲（内容已由 Message 记录承载或按策略丢弃）。"""
        self.journal.clear()
