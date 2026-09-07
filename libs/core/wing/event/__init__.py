# wing/event/__init__.py — 事件类型统一出口

"""
WingEvent 统一事件协议 (V2)。

所有事件均继承 WingEvent 基类，通过 type 字段区分。
V2 变更：
  - 删除 CreateSessionDoneEvent、SessionActivatedEvent、RewindDoneEvent、ForkDoneEvent
  - 新增 SyncSessionEvent、SessionStateChangedEvent
  - 拆分为四个模块：base、react、state_change、query_response
  - 删除 SystemEvent、SkillsListEvent、ShellCommandEvent（魔术命令消除后不再需要）
"""

from .base import (
    AgentInfo,
    CommandInfo,
    DeliveredEvent,
    ErrorEvent,
    EventTarget,
    WingEvent,
    SessionInfo,
    SessionStatus,
)
from .query_response import (
    BranchTargetInfo,
    BranchTargetsEvent,
    ContextStatsEvent,
)
from .react import (
    AskEvent,
    AssistantTurnEvent,
    DiffContentEvent,
    DoneEvent,
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
from .state_change import (
    CompactDoneEvent,
    InterruptedEvent,
    SessionInitEvent,
    SessionStateChangedEvent,
    SyncSessionEvent,
)

# 事件类型总集（便于类型检查）
WingEventUnion = (
    ErrorEvent
    | TextEvent
    | ReasoningEvent
    | ToolCallEvent
    | ToolCallStreamEvent
    | ToolCallResultEvent
    | LLMCallMetricsEvent
    | AskEvent
    | DoneEvent
    | TurnStartedEvent
    | UserMessageAcceptedEvent
    | DiffContentEvent
    | AssistantTurnEvent
    | ToolResultTurnEvent
    | TurnResultEvent
    | SyncSessionEvent
    | SessionInitEvent
    | DeliveredEvent
    | InterruptedEvent
    | CompactDoneEvent
    | SessionStateChangedEvent
    | ContextStatsEvent
    | BranchTargetsEvent
)

# 事件类型注册表：type 字面量 → 事件类。
# history.jsonl 加载时按 role="event" + type 在此分发还原事件节点
# （TrackedList.load）。未知 type 跳过——前向容忍。
EVENT_TYPES: dict[str, type[WingEvent]] = {
    "error": ErrorEvent,
    "text": TextEvent,
    "reasoning": ReasoningEvent,
    "tool_call": ToolCallEvent,
    "tool_call_stream": ToolCallStreamEvent,
    "tool_call_result": ToolCallResultEvent,
    "llm_call_metrics": LLMCallMetricsEvent,
    "ask": AskEvent,
    "done": DoneEvent,
    "turn_started": TurnStartedEvent,
    "user_message_accepted": UserMessageAcceptedEvent,
    "diff_content": DiffContentEvent,
    "assistant_turn": AssistantTurnEvent,
    "tool_result_turn": ToolResultTurnEvent,
    "turn_result": TurnResultEvent,
    "sync_session": SyncSessionEvent,
    "session_init": SessionInitEvent,
    "delivered": DeliveredEvent,
    "interrupted": InterruptedEvent,
    "compact_done": CompactDoneEvent,
    "session_state_changed": SessionStateChangedEvent,
    "context_stats": ContextStatsEvent,
    "branch_targets": BranchTargetsEvent,
}


def serialize_event(event: WingEvent) -> dict:
    """事件 → 传输/重放用的 dict（剥离传输路由元数据 target）。

    持久化记录与 SyncSessionEvent.events / in_flight 共用此形状：
    role="event" 记录级判别 + 事件自身字段。
    """
    return event.model_dump(exclude={"target"})


__all__ = [
    # base
    "WingEvent",
    "EventTarget",
    "AgentInfo",
    "CommandInfo",
    "SessionInfo",
    "SessionStatus",
    "ErrorEvent",
    "DeliveredEvent",
    # react
    "TextEvent",
    "ReasoningEvent",
    "ToolCallEvent",
    "ToolCallStreamEvent",
    "ToolCallResultEvent",
    "LLMCallMetricsEvent",
    "AskEvent",
    "DoneEvent",
    "TurnStartedEvent",
    "UserMessageAcceptedEvent",
    "DiffContentEvent",
    "AssistantTurnEvent",
    "ToolResultTurnEvent",
    "TurnResultEvent",
    # state_change
    "SyncSessionEvent",
    "SessionInitEvent",
    "InterruptedEvent",
    "CompactDoneEvent",
    "SessionStateChangedEvent",
    # query_response
    "ContextStatsEvent",
    "BranchTargetInfo",
    "BranchTargetsEvent",
    # union
    "WingEventUnion",
    # registry + serializer
    "EVENT_TYPES",
    "serialize_event",
]
