# wing/event/base.py — 基类、辅助 Schema、路由目标、通用系统事件

"""
WingEvent 基类与基础设施。

所有事件均继承 WingEvent 基类，通过 type 字段区分。
EventTarget 由 EventBus emit 时注入，Gateway 据此转发。
"""

from __future__ import annotations

import uuid
from datetime import datetime
from typing import ClassVar, Literal

from pydantic import BaseModel, Field

from wing.schema import ChainNode


# ============================================================
# 路由目标 — EventBus emit 时注入，Gateway 据此转发
# ============================================================


class EventTarget(BaseModel):
    """事件路由目标，由 EventBus.emit() 根据路由表计算并注入到事件中。

    scope:
      - "global"：发给所有 ws（如配置变更、全局通知）
      - "session"：发给订阅了该 session_id 的所有 client（EventBus 内部转换为 "client" + client_ids）
      - "client"：发给指定 client_ids 的 ws（投递者直接指定）

    投递者设置 scope="session" + event.session_id → EventBus 查路由表 →
    计算出 client_ids → 输出 scope="client" + client_ids 供 Gateway 转发。

    Gateway 只看到 "global" 或 "client" + client_ids，不感知 session_id。
    """

    scope: Literal["global", "session", "client"]
    client_ids: list[str] = Field(default_factory=list)


# ============================================================
# 辅助 Schema — 需要复用的结构化字段
# ============================================================


# Session 运行时状态（后端为唯一事实源）：
#   inactive — 在磁盘、未被 resume 进内存
#   idle     — 已 resume、agent 空闲
#   working  — agent 正在处理 turn
#   waiting  — 有工具阻塞在 feedback waiter，等待用户反馈
SessionStatus = Literal["inactive", "idle", "working", "waiting"]


class SessionInfo(BaseModel):
    """用于会话列表/详情中的 session 摘要信息。"""

    id: str
    name: str | None = None
    created_at: datetime | None = None
    template_name: str | None = None
    workspace: str | None = None
    last_interaction: str | None = None
    status: SessionStatus = "inactive"


class AgentInfo(BaseModel):
    """创建 agent 的完整配置，用于 Activate 时前端恢复 UI 状态。"""

    model_name: str
    system_prompt: str | None = None
    tools: list[str] = Field(default_factory=list)
    skills: list[str] = Field(default_factory=list)
    rules: list[str] = Field(default_factory=list)
    workspace: str | None = None


class CommandInfo(BaseModel):
    """魔术命令元信息，用于 HTTP GET /api/commands 端点。"""

    name: str
    aliases: list[str] = Field(default_factory=list)
    description: str = ""
    params: str = ""


# ============================================================
# 基类
# ============================================================


class WingEvent(ChainNode):
    """所有事件的基类。

    事件与 Message 共享链拓扑（ChainNode：uuid/parent_uuid），可混合进入
    TrackedList 活跃链——history.jsonl 的记录级判别靠 role 字段
    （Message 的 role ∈ user/assistant/tool/system，事件恒为 "event"）。

    - created_at：UTC datetime，人类可读，前端可反序列化。
    - type：子类必须覆盖为 Literal 字面量。
    - session_id：可选，某些事件在 session 创建前没有关联 session。
    - request_id：始终存在（自动生成 UUID），前端请求可覆写。
                  即使不是 RPC 响应，也始终存在，方便日志串联。
    - target：EventTarget，由 EventBus emit 时注入，Gateway 据此转发。
              传输路由元数据，不落盘（落盘记录在序列化时剥离）。
    - persist：是否持久化进 history.jsonl。ClassVar——不是 pydantic 字段，
      因此绝不进序列化（无 `Field(exclude=True)` 被子类重声明击穿的陷阱）。
      判据两条同时成立：**是事实**（读回来仍成立，非一次性信号）**且无
      Message 孪生**。true —— 完整产生时即时落盘进链（diff/ask/interrupted
      等权威记录）；false —— 只广播，不落盘、不缓冲（流式 delta 等瞬态内容
      由轮提交的 Message 记录承载，未提交内容由 accumulator 投影取得）。
      基类默认 True；需要不落盘的子类声明 `persist: ClassVar[bool] = False`。
      约束：ClassVar 不能按实例覆盖——全仓库无运行时 `persist=` 赋值/构造
      传参（已 grep 确认），代价今天为零；将来若需按实例覆盖，改回字段并在
      wire_dump / _to_record 显式排除。
    - disk_exclude：落盘记录排除的字段名（wire 帧与直播仍携带）。用于
      `result` 这类"只服务直播、落盘即孪生"的字段。ClassVar，不进序列化。
    """

    role: Literal["event"] = "event"
    created_at: datetime = Field(default_factory=lambda: datetime.now())
    type: str
    session_id: str | None = None
    request_id: str = Field(default_factory=lambda: uuid.uuid4().hex)
    target: EventTarget | None = None
    persist: ClassVar[bool] = True
    disk_exclude: ClassVar[frozenset[str]] = frozenset()


# ============================================================
# 通用系统事件
# ============================================================


class ErrorEvent(WingEvent):
    """错误事件，替代分散的 status_code 字段。

    成功事件不携带 status_code，失败事件走 ErrorEvent。
    SDK 的主 Promise 只由业务事件 resolve，由 ErrorEvent reject。
    """

    type: Literal["error"] = "error"
    status_code: int = 500
    message: str = ""
    error_code: str | None = None
    detail: str | None = None


class DeliveredEvent(WingEvent):
    """表示某个请求已被后端接受并投递到 agent / handler。

    所有 sendMessage / sendSteer 调用，后端都会立即回复此事件。
    SDK 的 Promise 解析策略：
    - sendMessage / sendSteer：DeliveredEvent resolve 主 Promise（仅确认送达）。
    - magic command：DeliveredEvent 仅表示送达，主 Promise 由后续特定事件 resolve。
    - ErrorEvent：reject 主 Promise。
    """

    type: Literal["delivered"] = "delivered"
    persist: ClassVar[bool] = False
