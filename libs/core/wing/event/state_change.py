# wing/event/state_change.py — 状态变更类事件

"""
状态变更事件：session 生命周期、压缩、中断等。
也包括 SyncSessionEvent——它同步 session 的完整状态，以及网关级的
SettingsChangedEvent（配置保存事务的回执广播）。
"""

from __future__ import annotations

import uuid as _uuid
from typing import Any, ClassVar, Literal

from pydantic import BaseModel, Field

from wing.store import SessionUsage

from .base import AgentInfo, HistoryNotice, SessionStatus, WingEvent
from .react import PendingUserMessage, ToolExecutionTiming

# ============================================================
# Session 生命周期事件
# ============================================================


class SessionHistory(BaseModel):
    """HTTP 首载、订阅快照与历史刷新的共同投影。

    依次组装 messages、uncommitted、uncommitted_tools、uncommitted_tool_results、
    events。pending_user_messages 中准备中的批次排在队首。turn_started_at 是
    当前 turn 的 UTC 起时，空闲时为 null。
    """

    messages: list[dict[str, Any]] = Field(default_factory=list)
    uncommitted: dict[str, Any] | None = None
    uncommitted_tools: list[dict[str, Any]] = Field(default_factory=list)
    uncommitted_tool_results: list[dict[str, Any]] = Field(default_factory=list)
    events: list[dict[str, Any]] = Field(default_factory=list)
    pending_user_messages: list[PendingUserMessage] = Field(default_factory=list)
    turn_started_at: str | None = None
    tool_execution_timings: dict[str, ToolExecutionTiming] = Field(default_factory=dict)


class _SessionSnapshot(WingEvent, SessionHistory):
    """订阅与主动刷新共用的快照，随后按 WS 到达序衔接 live 流。"""

    persist: ClassVar[bool] = False
    session_id: str
    status: SessionStatus
    agent: AgentInfo | None = None
    name: str | None = None
    draft: str | None = None
    usage: SessionUsage | None = None
    """快照时刻的会话用量账本（`Session.usage` 的全量投影）。

    这是前端**基线**的唯一来源：与快照同一时刻取值、同一条有序 WS 通道
    送达，快照之后的 live 事件恰是快照未包含的增量——「基线 = 快照值，
    增量 = 其后事件之和」因此精确成立（HTTP `/api/session/info` 的快照
    与 WS 帧无跨通道定序，不能拿来做基线，否则会把在途事件重复计入）。
    旧网关不带这个字段（None）——前端按零基线显示。"""


class SyncSessionEvent(_SessionSnapshot):
    type: Literal["sync_session"] = "sync_session"


class SessionHistoryRefreshedEvent(_SessionSnapshot):
    type: Literal["session_history_refreshed"] = "session_history_refreshed"


class SessionStateChangedEvent(WingEvent):
    """Session 级状态变更，只携带本次更新的值。

    模型变更的四个字段取自同一时刻，与 AgentInfo 同口径：`model_id` 是选择态引用词，
    `provider_name` 用于展示分组，`model` 是调用名，`model_display_name` 是可省略的展示名。
    """

    type: Literal["session_state_changed"] = "session_state_changed"
    persist: ClassVar[bool] = False
    model: str | None = None
    model_id: str | None = None
    provider_name: str | None = None
    model_display_name: str | None = None
    thinking: bool | None = None
    reasoning_effort: str | None = None
    yolo: bool | None = None
    title: str | None = None
    agent: str | None = None


class InterruptedEvent(HistoryNotice):
    type: Literal["interrupted"] = "interrupted"
    dropped_request_ids: list[str] = Field(default_factory=list)
    """打断丢弃的请求 ID，包含准备批次和打断入口处的积压输入。

    准备批次另有 `user_messages_dropped`，这里重复其 ID 供旧端清理。

    前端据此只把**这些** pending 消息标为 discarded；锁等待期间新投递的
    消息（客户端 POST 已应答）不在其中，留给重建后的消费者。旧网关不含
    此字段——前端按缺失（而非空列表）回落"全部丢弃"的兼容形态。"""


class CompactDoneEvent(HistoryNotice):
    type: Literal["compact_done"] = "compact_done"
    disk_exclude: ClassVar[frozenset[str]] = HistoryNotice.disk_exclude | {"summary"}
    summary: dict[str, Any] | None = None
    original_tokens: int
    compressed_tokens: int
    model: str = ""


# ============================================================
# 网关级状态变更（不属于任何 session）
# ============================================================


class SettingsChangedEvent(WingEvent):
    """网关配置已变更（``runtime.apply_settings`` 事务成功后广播）。

    **网关级通知，不属于任何会话**：``target = EventTarget(scope="global")``
    （所有已连接客户端都该知道配置变了——面板据此提示「配置已被其它客户端修改」）。

    不落盘也**不进** ``FACT_EVENTS``：它不是某个会话链上的事实，而是时点信号
    （"配置刚变了、新指纹是 X"）。客户端重连后正确的做法是重新 ``GET
    /api/settings/status``，重放一条旧通知只会误导（见 ``event/__init__.py``
    对两个集合的区分）。

    消费方不需要判断"这是不是我自己刚触发的那一次"——比指纹就够了：
    自己的保存会更新本地指纹，指纹相同即忽略。

    - ``changed`` / ``restart_required``：规范路径列表（不含值——值一律不进事件）。
    - ``setup_mode_exited``：这次保存是否让网关从 setup mode 转入正常模式（04 才可能为真）。
    - ``fingerprint``：落盘后的新指纹（乐观并发的比较基准）。
    """

    type: Literal["settings_changed"] = "settings_changed"
    persist: ClassVar[bool] = False
    changed: list[str]
    restart_required: list[str]
    setup_mode_exited: bool = False
    fingerprint: str


# ============================================================
# Session 初始化事件（stdio 模式）
# ============================================================


class SessionInitEvent(WingEvent):
    """订阅初始化，供 stdio 输出 `system/init`。

    `_push_sync()` 在 `replay=false` 时也发送本事件。`SyncSessionEvent` 随
    `replay=true` 发送，携带完整的会话快照。
    """

    type: Literal["session_init"] = "session_init"
    persist: ClassVar[bool] = False
    uuid: str = Field(default_factory=lambda: _uuid.uuid4().hex)
    tools: list[str] = Field(default_factory=list)
    model: str = ""
    permission_mode: str = "default"
    cwd: str = ""
