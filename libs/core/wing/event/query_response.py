# wing/event/query_response.py — 响应查询类事件

"""
查询响应事件：上下文统计、分支目标等。
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from .base import WingEvent


class ContextStatsEvent(WingEvent):
    """上下文统计信息——每次 _llm_turn 后 emit，前端据此更新状态栏。"""

    type: Literal["context_stats"] = "context_stats"
    persist: bool = False
    message_count: int
    total_tokens: int
    context_window_tokens: int = 0
    system_prompt_parts: list[str] = Field(default_factory=list)


class BranchTargetInfo(BaseModel):
    """单个可分叉/回退的消息节点信息。"""

    uuid: str
    content: str
    role: str = "user"


class BranchTargetsEvent(WingEvent):
    """可分叉/回退的用户消息列表，用于 /rewind 和 /fork 命令的候选面板。"""

    type: Literal["branch_targets"] = "branch_targets"
    persist: bool = False
    targets: list[BranchTargetInfo] = Field(default_factory=list)
