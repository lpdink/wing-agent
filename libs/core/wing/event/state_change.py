# wing/event/state_change.py — 状态变更类事件

"""
状态变更事件：session 生命周期、压缩、中断等。
也包括 SyncSessionEvent——它同步 session 的完整状态。
"""

from __future__ import annotations

import uuid as _uuid
from typing import Any, ClassVar, Literal

from pydantic import Field

from .base import AgentInfo, WingEvent


# ============================================================
# Session 生命周期事件
# ============================================================


class SyncSessionEvent(WingEvent):
    """同步 session 完整状态。

    由 /session 切换、/fork 触发。
    通知前端需要同步（replay）指定 session 的完整上下文。

    四组重放素材，订阅方 MUST 按 `messages → uncommitted → uncommitted_tools
    → events` 的顺序组装，随后无缝衔接 live 流：

    session_id：新 session 的 id（事件发给订阅新 session 的 client）
    messages：已提交 Message 投影列表（活跃链）
    uncommitted：**单个**未提交 assistant Message 投影（turn 进行中已终结块，
      可为 null）——与中断补提交同源（同一个 snapshot_blocks）
    uncommitted_tools：未终结 tool 调用的原始 args 片段列表
      （`[{tool_call_id, tool_name, args_fragment}]`），前端经既有 live
      ToolCallStream 分支局部解析渲染活工具卡
    events：活跃链上的**事实类**事件节点（按链序，过滤规则见 get_active_events）
    turn_started_at：当前 turn 的开始时刻（UTC ISO 字符串），无进行中 turn 时
      为 null——前端据此恢复已耗时而非从 resume 时刻重算
    agent：该 session 的 AgentInfo
    name：session 名称
    draft：用户还没发出去的草稿（rewind/fork 时可能有）
    """

    type: Literal["sync_session"] = "sync_session"
    persist: ClassVar[bool] = False
    session_id: str  # 新 session 的 id
    messages: list[dict[str, Any]] = Field(default_factory=list)
    uncommitted: dict[str, Any] | None = None
    uncommitted_tools: list[dict[str, Any]] = Field(default_factory=list)
    events: list[dict[str, Any]] = Field(default_factory=list)
    turn_started_at: str | None = None
    agent: AgentInfo | None = None
    name: str | None = None
    draft: str | None = None


class SessionStateChangedEvent(WingEvent):
    """统一的 session 级状态变更事件。

    替代 ModelSwitchedEvent + ThinkToggledEvent + SessionUpdatedEvent。
    所有字段可选，只携带当前值。
    """

    type: Literal["session_state_changed"] = "session_state_changed"
    persist: ClassVar[bool] = False
    model: str | None = None
    thinking: bool | None = None
    reasoning_effort: str | None = None
    yolo: bool | None = None
    title: str | None = None
    agent: str | None = None


class InterruptedEvent(WingEvent):
    type: Literal["interrupted"] = "interrupted"


class CompactDoneEvent(WingEvent):
    type: Literal["compact_done"] = "compact_done"
    original_tokens: int
    compressed_tokens: int
    model: str = ""


# ============================================================
# Session 初始化事件（stdio 模式）
# ============================================================


# TODO: SessionInitEvent 与 SyncSessionEvent 存在信息重叠（两者都携带 model、tools
# 等 session 状态）。当前阶段保持独立——前者面向 stdio 协议消费者，后者面向 TUI
# 状态同步。未来考虑是否统一。
class SessionInitEvent(WingEvent):
    """Session 初始化事件——供 stdio 模式输出 system/init 消息。

    在 subscribe/fork 后由 _push_sync() emit，携带权威的 session 状态。
    """

    type: Literal["session_init"] = "session_init"
    persist: ClassVar[bool] = False
    uuid: str = Field(default_factory=lambda: _uuid.uuid4().hex)
    tools: list[str] = Field(default_factory=list)
    model: str = ""
    permission_mode: str = "default"
    cwd: str = ""
