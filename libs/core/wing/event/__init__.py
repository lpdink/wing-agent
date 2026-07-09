# wing/event/__init__.py — 事件类型统一出口

"""
WingEvent 统一事件协议 (V2)。

所有事件均继承 WingEvent 基类，通过 type 字段区分。
V2 变更：
  - 删除 CreateSessionDoneEvent、SessionActivatedEvent、RewindDoneEvent、ForkDoneEvent
  - 新增 SyncSessionEvent、ModelListEvent
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
    AgentListEvent,
    BranchTargetInfo,
    BranchTargetsEvent,
    CommandListEvent,
    ContextStatsEvent,
    ModelListEvent,
    SkillsListEvent,
    ShellCommandEvent,
    SystemInfoEvent,
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
    ModelSwitchedEvent,
    SessionListEvent,
    SessionUpdatedEvent,
    SyncSessionEvent,
    ThinkToggledEvent,
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
    | SessionListEvent
    | SyncSessionEvent
    | DeliveredEvent
    | ModelSwitchedEvent
    | ThinkToggledEvent
    | InterruptedEvent
    | CompactDoneEvent
    | SessionUpdatedEvent
    | CommandListEvent
    | ContextStatsEvent
    | BranchTargetsEvent
    | ModelListEvent
    | AgentListEvent
    | SkillsListEvent
    | ShellCommandEvent
    | SystemInfoEvent
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
    "SessionListEvent",
    "SessionUpdatedEvent",
    "ModelSwitchedEvent",
    "ThinkToggledEvent",
    "InterruptedEvent",
    "CompactDoneEvent",
    # query_response
    "CommandListEvent",
    "ContextStatsEvent",
    "BranchTargetInfo",
    "BranchTargetsEvent",
    "ModelListEvent",
    "AgentListEvent",
    "SkillsListEvent",
    "ShellCommandEvent",
    "SystemInfoEvent",
    # union
    "WingEventUnion",
]
