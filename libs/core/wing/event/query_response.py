# wing/event/query_response.py — 响应查询类事件

"""
查询响应事件：前端请求后由 handler emit，用于候选面板等 UI 组件。
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from .base import CommandInfo, WingEvent


class CommandListEvent(WingEvent):
    """魔术命令列表，用于候选面板和 VSCode 命令面板。"""

    type: Literal["command_list"] = "command_list"
    commands: list[CommandInfo] = Field(default_factory=list)


class ContextStatsEvent(WingEvent):
    """上下文统计信息——每次 _llm_turn 后 emit，前端据此更新状态栏。"""

    type: Literal["context_stats"] = "context_stats"
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
    targets: list[BranchTargetInfo] = Field(default_factory=list)


class ModelListEvent(WingEvent):
    """可用模型列表，响应 /model 命令（无参）。"""

    type: Literal["model_list"] = "model_list"
    models: list[str] = Field(default_factory=list)
    current_model: str | None = None


class AgentListEvent(WingEvent):
    """可用 Agent 模板列表，响应 /agents 命令（无参）。"""

    type: Literal["agent_list"] = "agent_list"
    agents: list[str] = Field(default_factory=list)
    current_agent: str | None = None


class SkillsListEvent(WingEvent):
    """/skills 的结构化响应。V1 中走 SystemEvent，此为占位。"""

    type: Literal["skills_list"] = "skills_list"
    content: str
    skills: list[str] = Field(default_factory=list)


class ShellCommandEvent(WingEvent):
    """/bash 的结构化响应。V1 中走 SystemEvent，此为占位。"""

    type: Literal["shell_command"] = "shell_command"
    command: str = ""
    output: str = ""
    exit_code: int = 0


class SystemInfoEvent(WingEvent):
    """系统信息——TUI 启动时请求，前端据此初始化欢迎页和状态栏。

    由 /info 命令触发，携带当前 session 的完整运行时信息。
    """

    type: Literal["system_info"] = "system_info"
    model: str
    api_url: str = "unknown"
    tools: list[str] = Field(default_factory=list)
    total_tokens: int = 0
    context_window_tokens: int = 0
    thinking: bool = False
    session_name: str | None = None
