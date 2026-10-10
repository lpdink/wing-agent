# wing/agent/event_sink.py
"""AgentEventSink — agent 包内事件发射的唯一出口。

所有 react loop 期间的事件（流式文本、工具调用、turn 结果等）均通过
AgentEventSink 的方法发射。内部自动注入 session_id 和 EventTarget，
调用方无需关心路由细节。

持久化分流：
- persist=true → 经 append_event 落盘进混合链（即时，事件完整产生时刻）
  再广播——日志是事实源，广播是投影；
- persist=false → 纯广播，绝不落盘、不缓冲。流式 delta 等瞬态内容由轮
  提交时的 Message 记录承载；turn 进行中的未提交内容由 provider
  accumulator 投影（uncommitted_message / uncommitted_tools）按需取得，
  sink 不持有任何内存事件缓冲。

工具侧派生事件（DiffContentEvent 等经 ctx.emit / WingAgent.emit）同样
路由至此——sink 是唯一的发射出口，不存在绕过 sink 的直连 event_bus。

**上报窗口（`best_effort`）**：错误处理路径的收尾动作（turn_result / error /
done）与它报告的故障常常同源——落盘正是刚坏掉的资源（如 ENOSPC）。默认的
严格发射在这些路径上会**在 except 块里再抛一次**，同层接不住，异常逃出
run_turn / worker 循环（见 #187）。窗口把「报告动作不得失败」写成一段显式
契约：窗口内落盘失败降级为 ERROR 日志、事件仍尽力广播（前端靠它复位）。
"""

from __future__ import annotations

import contextlib
from collections.abc import Callable, Iterator
from typing import TYPE_CHECKING

from wing.common.logger import log
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
from wing.request_context import get_request_context

if TYPE_CHECKING:
    from wing.event import WingEvent
    from wing.schema import LLMUsage, MediaRef, Message, ToolCall


def emit_best_effort(
    event: WingEvent, append_event: Callable[[WingEvent], None] | None
) -> None:
    """降级发射原语：落盘失败降级为 ERROR 日志，事件仍**尽力广播**，绝不抛出。

    只做「落盘（persist=true 且给了写口时）→ 广播」两件事；元数据定型
    （request_id / target）由调用方在此之前完成——`AgentEventSink._prepare`
    与 `WingRuntime._emit_session_event` 的定型口径不同，不在这里合并。

    用在**错误 / 上报路径**上（`AgentEventSink.best_effort()` 窗口、runtime
    的 session 级上报）：这些动作与它们报告的故障常常同源——落盘正是刚坏掉
    的资源（如 ENOSPC）。严格发射会让异常在 except 块里再抛一次（同层接不
    住），或者让**已经生效**的业务动作（打断 / 回退 / 压缩）对客户端表现为
    失败（500 + 事件永不下发），两者都不允许。

    已知代价：磁盘上没有这条记录（报告是记账，不是事实）——前端靠广播拿到
    它复位状态，resume 重放以磁盘为准。
    """
    if event.persist and append_event is not None:
        try:
            append_event(event)
        except Exception:
            log.exception(
                f"事件上报落盘失败（已降级；仍向客户端广播）："
                f"type={type(event).__name__}"
            )
    try:
        event_bus.emit(event)
    except Exception:
        log.exception(f"事件上报广播失败（已降级）：type={type(event).__name__}")


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
        # 降级发射窗口（见 `best_effort`）：只由错误上报路径进入。
        self._best_effort = False

    @contextlib.contextmanager
    def best_effort(self) -> Iterator[None]:
        """上报窗口：窗口内的发射失败降级为日志，绝不抛出（#187）。

        兜底 handler 的收尾（`turn_result` / `error` / `done`）依赖落盘，而
        落盘可能正是它正在报告的故障：严格发射会在 except 块里再抛一次，
        同层接不住——异常逃出 `run_turn`、再逃出 worker 的 while True，
        消费者协程就此结束（inbox 里的消息再无人消费 = 僵尸态）。

        窗口内 `emit` 的契约：

        - 落盘失败 → ERROR 日志（含完整异常链），继续；
        - 仍**尽力广播**事件——前端靠它复位 working 态（磁盘上没有这条记录
          是已知代价：报告是记账，不是事实）；
        - 广播失败 → ERROR 日志，继续。

        窗口是 sink 上的瞬时状态，用 try/finally 收口。窗口内**不得有
        await**：没有 yield point 就不可能串到别的任务，窗口不会漏到窗口外
        的发射上（嵌套时按栈恢复原值）。
        """
        previous = self._best_effort
        self._best_effort = True
        try:
            yield
        finally:
            self._best_effort = previous

    def emit(self, event: WingEvent) -> None:
        """发射事件（严格模式：落盘失败即抛，由调用方决定语义）。

        错误上报路径用 `best_effort()` 窗口包住调用，把「报告不得失败」的
        契约写在调用点上。
        """
        if self._best_effort:
            self._emit_degraded(event)
        else:
            self._emit_strict(event)

    def _emit_strict(self, event: WingEvent) -> None:
        self._prepare(event)
        if event.persist and self._append_event is not None:
            self._append_event(event)
        event_bus.emit(event)

    def _emit_degraded(self, event: WingEvent) -> None:
        """上报窗口内的发射：任何一步失败都降级为日志，绝不抛出。"""
        try:
            self._prepare(event)
        except Exception:
            log.exception(f"事件上报定型失败（已降级）：type={type(event).__name__}")
            # 定型失败仍要尽力广播（报告不得丢），但不能落进 EventBus 对
            # `target=None` 的 global 兜底——一条 session 定向的收尾不该发给
            # 所有 client：补最小定向。
            if event.target is None:
                event.target = EventTarget(scope="session")
        emit_best_effort(event, self._append_event)

    def _prepare(self, event: WingEvent) -> None:
        # 关联元数据定型：在落盘之前完成 request_id 注入，保证磁盘记录
        # 与广播帧携带同一个值（日志是唯一事实来源——live replay 与
        # resume replay 不允许对同一事件呈现不同的 request_id）。
        # 注：request_id 是关联标记（correlation id）而非链拓扑身份——
        # 身份（uuid/parent_uuid）由 TrackedList 后端生成，与此无关。
        ctx = get_request_context()
        if ctx.request_id is not None:
            event.request_id = ctx.request_id
        if event.session_id is None:
            event.session_id = self._session_id
        if event.target is None:
            event.target = EventTarget(scope="session")

    # ── Turn 生命周期 ──

    def turn_started(self) -> None:
        self.emit(TurnStartedEvent(session_id=self._session_id))

    def user_message_accepted(
        self, content: str, origin_request_id: str | None
    ) -> None:
        """用户消息被消费进模型上下文（新 turn 输入或 steer 注入）。"""
        self.emit(
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
        self.emit(
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
        self.emit(DoneEvent(session_id=self._session_id))

    def error(self, message: str) -> None:
        self.emit(ErrorEvent(session_id=self._session_id, message=message))

    # ── Assistant turn ──

    def assistant_turn(self, msg: Message, model: str) -> None:
        self.emit(AssistantTurnEvent.from_message(msg, model, self._session_id))

    # ── Context stats ──

    def context_stats(
        self, message_count: int, total_tokens: int, context_window_tokens: int
    ) -> None:
        self.emit(
            ContextStatsEvent(
                session_id=self._session_id,
                message_count=message_count,
                total_tokens=total_tokens,
                context_window_tokens=context_window_tokens,
            )
        )

    # ── Tool events ──

    def tool_started(self, tc: ToolCall) -> None:
        self.emit(ToolCallEvent.from_tool_call(tc, self._session_id))

    def tool_finished(
        self,
        tc: ToolCall,
        result: str,
        *,
        success: bool,
        model: str,
        media: list[MediaRef] | None = None,
    ) -> None:
        """一次调用发射 ToolCallResultEvent + ToolResultTurnEvent。

        media 只进 ToolCallResultEvent（tool_media 字段）——ToolResultTurnEvent
        是 Claude SDK 兼容形状（纯文本），不扩展。
        """
        self.emit(
            ToolCallResultEvent.from_execution(
                tc, result, success, model, self._session_id, media=media
            )
        )
        self.emit(
            ToolResultTurnEvent.from_execution(tc, result, success, self._session_id)
        )

    # ── LLM streaming events ──

    def llm_text(self, content: str) -> None:
        self.emit(TextEvent(session_id=self._session_id, content=content))

    def llm_reasoning(self, content: str) -> None:
        self.emit(ReasoningEvent(session_id=self._session_id, content=content))

    def llm_tool_call_delta(
        self, tool_call_id: str, tool_name: str, args_fragment: str, is_final: bool
    ) -> None:
        self.emit(
            ToolCallStreamEvent(
                session_id=self._session_id,
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                args_fragment=args_fragment,
                is_final=is_final,
            )
        )

    def llm_metrics(self, usage: LLMUsage) -> None:
        self.emit(
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
