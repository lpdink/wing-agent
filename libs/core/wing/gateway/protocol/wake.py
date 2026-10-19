# wing/gateway/protocol/wake.py — 唤醒端点（``/api/wake/*``）的 wire 模型

"""唤醒触发器的 HTTP 协议：创建 / 列表 / 取消。

形状与领域对象一一对应，但**时间是人可读的本地 ISO 字符串**（与 ``last_turn_end`` /
链记录的 ``ts`` 同口径），库里仍是 epoch 毫秒。转换只发生在 ``gateway/projection.py``。
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

WakeKind = Literal["session", "timer"]


class WakeCreateRequest(BaseModel):
    """``POST /api/wake/create`` 请求。

    - ``kind=session``：由 ``source_session_id`` 的事件驱动（``on_events`` 缺省 ``done,ask``）；
      ``times`` 是 done 信封的配额（缺省 1，即「盯这一次收口」）。
    - ``kind=timer``：由 ``spec`` 驱动（``in:30m`` / ``at:<local iso>`` / ``every:30m`` /
      ``daily:07:00``），``prompt`` 必填；``times`` 只对周期型 spec 合法。
    """

    kind: WakeKind = Field(
        description="触发器种类：session（事件驱动）| timer（时间驱动）"
    )
    target_session_id: str = Field(description="唤醒谁（收到消息的会话）")
    source_session_id: str | None = Field(
        default=None, description="订阅谁的事件（kind=session 必填）"
    )
    on_events: str | None = Field(
        default=None,
        description="逗号列表，取值 done,ask（缺省两者都开）",
    )
    spec: str | None = Field(
        default=None,
        description="时间 spec（kind=timer 必填）：in:30m | at:<local iso> | every:30m | daily:07:00",
    )
    prompt: str | None = Field(
        default=None,
        description=(
            "kind=timer 必填（原文发送）；kind=session 给定时替换内置信封（原样发送）"
        ),
    )
    times: int | None = Field(
        default=None,
        description=(
            "session：done 信封配额（缺省 1）；timer：触发次数（缺省无限；"
            "只对 every/daily 合法）"
        ),
    )


class WakeInfo(BaseModel):
    """一条触发器的对外快照（时间字段是本地 naive ISO，``None`` = 不适用）。"""

    id: int = Field(description="触发器 id（取消时用）")
    kind: str = Field(description="session | timer")
    target_session_id: str = Field(description="收到唤醒消息的会话")
    source_session_id: str | None = Field(default=None, description="被订阅的会话")
    on_events: str = Field(description="done,ask 的子集")
    spec: str | None = Field(default=None, description="时间 spec（timer 才有）")
    prompt: str | None = Field(default=None, description="用户自定义投递内容")
    state: str = Field(description="active | done | canceled | failed")
    fire_count: int = Field(description="已经成功投递的次数")
    last_fire_at: str | None = Field(default=None, description="最近一次成功投递的时刻")
    last_error: str | None = Field(
        default=None, description="失败原因（state=failed 时）"
    )
    created_at: str | None = Field(default=None, description="创建时刻")
    next_fire_at: str | None = Field(default=None, description="下一次定时触发时刻")


class WakeCreateResponse(BaseModel):
    """``POST /api/wake/create`` 响应。"""

    wake: WakeInfo


class WakeListResponse(BaseModel):
    """``GET /api/wake/list`` 响应（``?all=true`` 含已终结的）。"""

    wakes: list[WakeInfo] = Field(default_factory=list)


class WakeCancelRequest(BaseModel):
    """``POST /api/wake/cancel`` 请求。"""

    id: int = Field(description="触发器 id")


class WakeCancelResponse(BaseModel):
    """``POST /api/wake/cancel`` 响应。

    ``canceled=false`` 是**幂等 no-op**（触发器早已终结），不是错误；id 不存在才是 404。
    """

    ok: bool = Field(default=True, description="请求是否被执行")
    canceled: bool = Field(description="本次调用是否真的取消了它")
