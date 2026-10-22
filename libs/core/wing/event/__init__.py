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
#
# 从类派生：`type` 的唯一声明处是每个类的字面量字段。新增事件类只要被本模块
# import 到就自动入表；`tests/test_event_registry.py` 用 AST 扫源码兜住
# "新类忘了被 import"这条派生看不到的失效。
def _derived_event_types() -> dict[str, type[WingEvent]]:
    """递归遍历 WingEvent 的全部子类，取 `type` 字段的默认值建表。

    没有默认值的类（基类 / 只声明 `type: str` 的抽象中间类）跳过。
    """
    found: dict[str, type[WingEvent]] = {}
    stack = list(WingEvent.__subclasses__())
    while stack:
        cls = stack.pop()
        stack.extend(cls.__subclasses__())
        type_name = cls.model_fields["type"].default
        if isinstance(type_name, str):
            found[type_name] = cls
    return found


EVENT_TYPES: dict[str, type[WingEvent]] = _derived_event_types()
# `settings_changed` 也在表里（由上面的派生得到）：它是网关级通知（global scope，
# 不属于任何会话链），登记只为「type → 类」的完整性；它 persist=False，永不进链、
# 因此也不进 resume 重放。


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
#
# 成员由 `tests/test_event_registry.py` 钉住：本集合 == persist=True 的 type
# 减去「落盘但不重放」的轮级 / 一次性信号（那张负表也在这个测试里）。

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
