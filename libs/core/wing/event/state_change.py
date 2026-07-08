# wing/event/state_change.py — 状态变更类事件

"""
状态变更事件：session 生命周期、模型切换、think 模式、压缩、中断等。
也包括 SyncSessionEvent——它同步 session 的完整状态。
"""

from __future__ import annotations

from typing import Any, Literal

from pydantic import Field

from .base import AgentInfo, WingEvent


# ============================================================
# Session 生命周期事件
# ============================================================


class SyncSessionEvent(WingEvent):
    """同步 session 完整状态。

    由 /session 切换、/fork 触发。
    通知前端需要同步（replay）指定 session 的完整上下文。

    session_id：新 session 的 id（事件发给订阅新 session 的 client）
    messages：完整上下文消息列表
    agent：该 session 的 AgentInfo
    name：session 名称
    draft：用户还没发出去的草稿（rewind/fork 时可能有）
    """

    type: Literal["sync_session"] = "sync_session"
    session_id: str  # 新 session 的 id
    messages: list[dict[str, Any]] = Field(default_factory=list)
    agent: AgentInfo | None = None
    name: str | None = None
    draft: str | None = None


class SessionListEvent(WingEvent):
    """返回当前磁盘上存在的所有会话列表。

    响应 /session 命令（无参）。
    """

    type: Literal["session_list"] = "session_list"
    sessions: list[Any] = Field(default_factory=list)  # list[SessionInfo]


class SessionUpdatedEvent(WingEvent):
    """session 属性更新，例如重命名。"""

    type: Literal["session_updated"] = "session_updated"
    name: str | None = None


# ============================================================
# 模型与模式变更
# ============================================================


class ModelSwitchedEvent(WingEvent):
    type: Literal["model_switched"] = "model_switched"
    old_model: str
    new_model: str


class ThinkToggledEvent(WingEvent):
    """思考模式切换（/think on|off）。"""

    type: Literal["think_toggled"] = "think_toggled"
    enabled: bool


class InterruptedEvent(WingEvent):
    type: Literal["interrupted"] = "interrupted"


class CompactDoneEvent(WingEvent):
    type: Literal["compact_done"] = "compact_done"
    original_tokens: int
    compressed_tokens: int
    model: str = ""
