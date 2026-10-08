# wing/event/state_change.py — 状态变更类事件

"""
状态变更事件：session 生命周期、压缩、中断等。
也包括 SyncSessionEvent——它同步 session 的完整状态。
"""

from __future__ import annotations

import uuid as _uuid
from typing import Any, ClassVar, Literal

from pydantic import Field

from .base import AgentInfo, SessionStatus, WingEvent


# ============================================================
# Session 生命周期事件
# ============================================================


class SyncSessionEvent(WingEvent):
    """同步 session 完整状态。

    由 /session 切换、/fork 触发。
    通知前端需要同步（replay）指定 session 的完整上下文。

    状态（快照事实）：
    status：快照时刻 session 的运行状态（idle / working / waiting），与
      `/api/session/list` 同一取值域（`inactive` 不会出现——能同步的 session
      必在内存）。**必填**：快照必须自述状态，订阅方直接读它（working / waiting
      ⇒ turn 在飞行），MUST NOT 由 uncommitted / uncommitted_tools 是否为空反推
      ——"有未提交内容 ⇒ working" 只单向成立（一轮 LLM 调用在飞行、尚未吐出首个
      已终结块时两者都空，而 turn 确在 working）。也不存在"回落推断"这条路径：
      CLI 与网关同版本升级，字段缺失 / 取值未知即协议错误（两端解码期失败，而
      非静默退化）。
    turn_started_at：当前 turn 的开始时刻（UTC ISO 字符串），无进行中 turn 时
      为 null——前端据此恢复已耗时而非从 resume 时刻重算

    四组重放素材，订阅方 MUST 按 `messages → uncommitted → uncommitted_tools
    → events` 的顺序组装，随后无缝衔接 live 流。快照与 live 事件 MUST 按到达序
    应用（网关的送达序 == 发射序；乱序会让快照覆盖更晚的 live 状态）：

    session_id：新 session 的 id（事件发给订阅新 session 的 client）
    messages：已提交 Message 投影列表（活跃链）
    uncommitted：**单个**未提交 assistant Message 投影（turn 进行中已终结块，
      可为 null）——与中断补提交同源（同一个 snapshot_blocks）
    uncommitted_tools：未终结 tool 调用的原始 args 片段列表
      （`[{tool_call_id, tool_name, args_fragment}]`），前端经既有 live
      ToolCallStream 分支局部解析渲染活工具卡
    events：活跃链上的**事实类**事件节点（按链序，过滤规则见 get_active_events）
    agent：该 session 的 AgentInfo
    name：session 名称
    draft：用户还没发出去的草稿（rewind/fork 时可能有）
    """

    type: Literal["sync_session"] = "sync_session"
    persist: ClassVar[bool] = False
    session_id: str
    messages: list[dict[str, Any]] = Field(default_factory=list)
    uncommitted: dict[str, Any] | None = None
    uncommitted_tools: list[dict[str, Any]] = Field(default_factory=list)
    events: list[dict[str, Any]] = Field(default_factory=list)
    # 无默认值：每个构造点都必须显式给出快照时刻的状态——同步载荷自描述，
    # 前端不做推断（漏给值应当是构造期的错误，而非静默退化成"不在跑"）。
    status: SessionStatus
    turn_started_at: str | None = None
    agent: AgentInfo | None = None
    name: str | None = None
    draft: str | None = None


class SessionStateChangedEvent(WingEvent):
    """统一的 session 级状态变更事件。

    替代 ModelSwitchedEvent + ThinkToggledEvent + SessionUpdatedEvent。
    所有字段可选，只携带当前值。

    模型三件套与 AgentInfo 对齐：``model_id`` 是引用词（前端据此做选择态匹配），
    ``provider_name`` 是运行期事实（展示分组），``model`` 是调用名 + 展示名
    ``model_display_name``。四者同刻下发（model_id 未取到时为 None）。
    """

    type: Literal["session_state_changed"] = "session_state_changed"
    persist: ClassVar[bool] = False
    model: str | None = None
    model_id: str | None = None
    provider_name: str | None = None
    """当前模型的 provider 名（运行期事实；与 model / model_id 同刻下发）。"""
    model_display_name: str | None = None
    """当前模型的展示名（与 model 同刻下发；未声明 / 无展示名 = 省略，
    前端回落 model）。"""
    thinking: bool | None = None
    reasoning_effort: str | None = None
    yolo: bool | None = None
    title: str | None = None
    agent: str | None = None


class InterruptedEvent(WingEvent):
    type: Literal["interrupted"] = "interrupted"
    dropped_request_ids: list[str] = Field(default_factory=list)
    """打断入口放弃的积压输入（`request_id`，队列序）。

    前端据此只把**这些** pending 消息标为 discarded；锁等待期间新投递的
    消息（客户端 POST 已应答）不在其中，留给重建后的消费者。旧网关不含
    此字段——前端按缺失（而非空列表）回落"全部丢弃"的兼容形态。"""


class CompactDoneEvent(WingEvent):
    type: Literal["compact_done"] = "compact_done"
    original_tokens: int
    compressed_tokens: int
    model: str = ""


# ============================================================
# Session 初始化事件（stdio 模式）
# ============================================================


# TODO: SessionInitEvent 与 SyncSessionEvent 信息重叠（都携带 model / tools 等
# session 状态）——前者面向 stdio 协议消费者，后者面向 TUI 状态同步。
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
