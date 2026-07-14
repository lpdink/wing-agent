# wing/event/__init__.py — 事件类型统一出口

"""
WingEvent 统一事件协议 (V2)。

所有事件均继承 WingEvent 基类，通过 type 字段区分。
V2 变更：
  - 删除 CreateSessionDoneEvent、SessionActivatedEvent、RewindDoneEvent、ForkDoneEvent
  - 新增 SyncSessionEvent、SessionStateChangedEvent
  - 拆分为四个模块：base、react、state_change、query_response
"""

from .base import (
    AgentInfo,
    CommandInfo,
    DeliveredEvent,
    ErrorEvent,
    EventTarget,
    WingEvent,
    SessionInfo,
    SystemEvent,
)
from .query_response import (
    BranchTargetInfo,
    BranchTargetsEvent,
    ContextStatsEvent,
    SkillsListEvent,
    ShellCommandEvent,
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
    SystemEvent
    | ErrorEvent
    | TextEvent
    | ReasoningEvent
    | ToolCallEvent
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
    | SkillsListEvent
    | ShellCommandEvent
)

__all__ = [
    # base
    "WingEvent",
    "EventTarget",
    "AgentInfo",
    "CommandInfo",
    "SessionInfo",
    "SystemEvent",
    "ErrorEvent",
    "DeliveredEvent",
    # react
    "TextEvent",
    "ReasoningEvent",
    "ToolCallEvent",
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
    "SkillsListEvent",
    "ShellCommandEvent",
    # union
    "WingEventUnion",
]
