# wing/event/__init__.py — 事件类型统一出口

"""
WingEvent 统一事件协议。

所有事件均继承 WingEvent 基类，通过 type 字段区分，按域拆分为四个模块：
base（基类 / 辅助 Schema / 通用系统事件）、react（ReAct 循环事件流）、
state_change（会话状态变更）、query_response（查询响应）。
"""

from .base import (
    AgentInfo,
    CommandInfo,
    DeliveredEvent,
    ErrorEvent,
    EventTarget,
    NoticeEvent,
    SessionInfo,
    SessionStatus,
    WingEvent,
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
    SettingsChangedEvent,
    SyncSessionEvent,
)

# 事件类型注册表：type 字面量 → 事件类。
# history.jsonl 加载时按 role="event" + type 在此分发还原事件节点
# （TrackedList.load）。未知 type 跳过——前向容忍。
EVENT_TYPES: dict[str, type[WingEvent]] = {
    "error": ErrorEvent,
    "notice": NoticeEvent,
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
    # 网关级通知（global scope，不属于任何会话链）：登记只为「type → 类」的完整性；
    # 它 persist=False，永不进链、因此也不进 resume 重放。
    "settings_changed": SettingsChangedEvent,
}


# 事实事件集合：persist=true 且无 Message 孪生的"事实类"事件——resume 时
# 下发给前端重放（ContextManager.get_active_events 据此过滤）。与 persist
# 标记同处一文件，过滤策略单点。存量日志里已写入的孪生记录
# （tool_call_result / llm_call_metrics）不在此集合：加载进链但不下发。
FACT_EVENTS: frozenset[str] = frozenset(
    {
        "diff_content",
        "ask",
        "interrupted",
        "error",
        "compact_done",
    }
)
# 集合的成员条件（两条同时成立）：事件**落在某个会话的链上**（TrackedList）
# **且**是读回来仍成立的事实。settings_changed 两条都不满足——它是网关级
# 时点通知（无 session_id、不进任何链），重连后的正解是重新 GET
# /api/settings/status，而不是重放一条旧通知。

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
    "NoticeEvent",
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
    "SettingsChangedEvent",
    # query_response
    "ContextStatsEvent",
    "BranchTargetInfo",
    "BranchTargetsEvent",
    # registry + serializer
    "EVENT_TYPES",
    "FACT_EVENTS",
    "wire_dump",
]
