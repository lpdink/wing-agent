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
]
