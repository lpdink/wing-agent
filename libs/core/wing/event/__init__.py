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
    """事件 → 传输/重放用的 dict（统一 wire 规则，`wire_dump` 的别名）。

    保留此名以兼容既有调用方（runtime 的 SyncSession.events 组装）；
    实现与直播帧共用 `wire_dump`，不存在两套序列化形状。
    """
    return wire_dump(event)


# 事实事件集合：persist=true 且无 Message 孪生的"事实类"事件——resume 时
# 下发给前端重放（ContextManager.get_active_events 据此过滤）。与 persist
# 标记同处一文件，过滤策略单点。存量日志里已写入的孪生记录
# （tool_call_result / llm_call_metrics）不在此集合：加载进链但不下发。
# 终态（follow-up）：待信号类事件的落盘取舍定完，FACT_EVENTS 与 persist=true
# 集合重合，下发过滤即消失。本次不宣称达到终态，只把策略从两端收敛到后端单点。
FACT_EVENTS: frozenset[str] = frozenset(
    {
        "diff_content",
        "ask",
        "interrupted",
        "error",
        "compact_done",
    }
)

# wire 帧 / SyncSession 载荷剥除的存储专用字段：parent_uuid / unzip_last_uuid
# 是链拓扑（仅磁盘记录需要），role 是记录级判别（恒为 "event"，无信息），
# target 是 EventBus 注入的传输路由元数据（gateway 消费后不进帧）。
# uuid 保留——AssistantTurnEvent / TurnResultEvent 的 uuid 被 stdio 前端消费。
_WIRE_EXCLUDE: set[str] = {"target", "parent_uuid", "unzip_last_uuid", "role"}


def wire_dump(event: WingEvent) -> dict:
    """事件 → WS 直播帧 / SyncSession 载荷的统一 wire dict。

    剥除存储专用字段（`_WIRE_EXCLUDE`）与值为 null 的字段；保留 uuid。
    直播帧（gateway server / ws）、SyncSession.events、SyncSession.uncommitted
    共用此规则——消除各调用点分散的 `model_dump_json`。事件无 wrap 序列化器，
    `model_dump(mode="json", exclude=...)` 后字典过滤 null 即可（与 Message 的
    存储边界不同，见 TrackedList._to_record）。
    """
    data = event.model_dump(mode="json", exclude=_WIRE_EXCLUDE)
    return {k: v for k, v in data.items() if v is not None}


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
    "FACT_EVENTS",
    "serialize_event",
    "wire_dump",
]
