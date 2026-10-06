# wing/gateway/projection.py — 领域对象 → 协议响应模型的投影

"""Session 端点响应的投影——领域对象 → 协议模型。

``/api/session/info`` 与 ``/api/session/branches`` 的响应要读 session 的内部件
（agent 运行时状态 / 上下文统计 / skills / system prompt / 分支候选）：投影集中在
这里，路由只做「参数校验 → 调 runtime → 构造响应」，不再穿透 ``session.agent.*``。
投影是纯读——不修改 session 状态。
"""

from __future__ import annotations

from wing.event.query_response import BranchTargetInfo
from wing.gateway.protocol import (
    BranchesResponse,
    ContextStatsInfo,
    SessionInfoResponse,
)
from wing.session import Session


def build_session_info(session: Session) -> SessionInfoResponse:
    """GET /api/session/info 响应——session 运行时状态快照。"""
    status = session.agent.get_status()
    cm = session.agent.context_manager
    msg_count, total_tok = cm.get_context_stats()
    return SessionInfoResponse(
        model=status["model"],
        model_display_name=session.agent.model_display_name,
        api_url=status["api_url"],
        tools=status["tools"],
        total_tokens=status["total_tokens"],
        context_window_tokens=status["context_window_tokens"],
        thinking=status["thinking"],
        reasoning_effort=status["reasoning_effort"],
        yolo=session.agent.yolo,
        session_name=session.session_name,
        workdir=session.session_workspace,
        status=session.status,
        context_stats=ContextStatsInfo(
            message_count=msg_count,
            total_tokens=total_tok,
        ),
        skills_info=cm.get_skills_info(),
        system_prompt=cm.system_prompt.content or "",
        tags=session.tags,
    )


def build_session_branches(session: Session) -> BranchesResponse:
    """GET /api/session/branches 响应——可回退 / 分叉的消息节点。"""
    raw_targets = session.agent.context_manager.get_branch_targets()
    targets = [BranchTargetInfo(**t) for t in raw_targets]
    return BranchesResponse(targets=targets)
