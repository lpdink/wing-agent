# wing/gateway/projection.py — 领域对象 → 协议响应模型的投影

"""Session / Setting API 端点响应的投影——领域对象 → 协议模型。

``/api/session/info`` 与 ``/api/session/branches`` 的响应要读 session 的内部件
（agent 运行时状态 / 上下文统计 / skills / system prompt / 分支候选）：投影集中在
这里，路由只做「参数校验 → 调 runtime → 构造响应」，不再穿透 ``session.agent.*``。
投影是纯读——不修改 session 状态。

Setting API 的部分同理（03 追加）：``config.SettingNode`` → ``SettingNodeProto``、
``config.ConfigProblem`` → ``SettingProblem``、``document.SecretState`` → 协议
``SecretState``、``runtime.SettingsApplyResult`` → ``SettingsSetResponse``。
领域类型住 L3、wire 类型住 L4，**这里是唯一的转换点**（照 ``_model_detail`` 的既有模式）。
"""

from __future__ import annotations

from wing.config import ConfigProblem, SettingNode
from wing.config.document import SecretState as SecretStateDomain
from wing.event.query_response import BranchTargetInfo
from wing.gateway.protocol import (
    BranchesResponse,
    ContextStatsInfo,
    ReloadResponse,
    ReloadResultItem,
    SecretState as SecretStateProto,
    SessionInfoResponse,
    SettingChoice,
    SettingNodeProto,
    SettingProblem,
    SettingsSchemaResponse,
    SettingsSetResponse,
)
from wing.runtime import SettingsApplyResult
from wing.session import Session
from wing.system import ReloadResult


def build_session_info(session: Session) -> SessionInfoResponse:
    """GET /api/session/info 响应——session 运行时状态快照。"""
    status = session.agent.get_status()
    cm = session.agent.context_manager
    msg_count, total_tok = cm.get_context_stats()
    return SessionInfoResponse(
        model=status["model"],
        model_id=session.model_id,
        provider_name=session.agent.provider_name,
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
        tag_meta=session.tag_meta,
    )


def build_session_branches(session: Session) -> BranchesResponse:
    """GET /api/session/branches 响应——可回退 / 分叉的消息节点。"""
    raw_targets = session.agent.context_manager.get_branch_targets()
    targets = [BranchTargetInfo(**t) for t in raw_targets]
    return BranchesResponse(targets=targets)


# ============================================================
# Setting API（03）
# ============================================================


def build_settings_node(node: SettingNode) -> SettingNodeProto:
    """catalog 节点 → wire 节点（递归；字段 1:1，不做语义加工）。"""
    return SettingNodeProto(
        key=node.key,
        path=node.path,
        title=node.title,
        doc=node.doc,
        notes=list(node.notes),
        example=node.example,
        order=node.order,
        kind=node.kind,
        required=node.required,
        nullable=node.nullable,
        default=node.default,
        has_default=node.has_default,
        min=node.min,
        max=node.max,
        exclusive_min=node.exclusive_min,
        exclusive_max=node.exclusive_max,
        min_length=node.min_length,
        pattern=node.pattern,
        choices=[SettingChoice(value=c.value, doc=c.doc) for c in node.choices],
        min_items=node.min_items,
        max_items=node.max_items,
        secret=node.secret,
        apply=node.apply,
        editable=node.editable,
        deprecated=node.deprecated,
        section=node.section,
        section_doc=node.section_doc,
        children=[build_settings_node(child) for child in node.children],
        element=build_settings_node(node.element) if node.element else None,
        variants=(
            [build_settings_node(v) for v in node.variants]
            if node.variants is not None
            else None
        ),
        summary_fields=list(node.summary_fields),
        value_hint=node.value_hint,
    )


def build_settings_schema(
    catalog: SettingNode, *, version: str, config_path: str
) -> SettingsSchemaResponse:
    """GET /api/settings/schema 响应——目录树 + 网关版本 + 文件绝对路径。"""
    return SettingsSchemaResponse(
        version=version,
        root=build_settings_node(catalog),
        config_path=config_path,
    )


def build_settings_problem(problem: ConfigProblem) -> SettingProblem:
    """``ConfigProblem`` → wire 问题（``kind`` 序列化为 enum 的 value 字符串，增补 P6）。"""
    return SettingProblem(
        path=problem.path,
        kind=problem.kind.value,
        message=problem.message,
        hint=problem.hint,
    )


def build_settings_secret_states(
    states: dict[str, SecretStateDomain],
) -> dict[str, SecretStateProto]:
    """密文状态表 → wire 形状（键 = 规范路径）。"""
    return {
        path: SecretStateProto(state=state.state, hint=state.hint)
        for path, state in states.items()
    }


def build_reload_response(result: ReloadResult) -> ReloadResponse:
    """``ReloadResult`` → wire 形状（复用 ``/api/system/reload`` 的既有形状）。

    既有 reload handler 内联构造同形响应；这里提供给新的调用点（保存回执），
    形状由 protocol 冻结，两处不会有语义漂移。
    """
    return ReloadResponse(
        ok=result.ok,
        results=[
            ReloadResultItem(name=item.name, ok=item.ok, detail=item.detail)
            for item in result.items
        ],
    )


def build_settings_apply_response(result: SettingsApplyResult) -> SettingsSetResponse:
    """``SettingsApplyResult`` → POST /api/settings/set 的回执。"""
    return SettingsSetResponse(
        ok=result.ok,
        fingerprint=result.fingerprint,
        problems=[build_settings_problem(p) for p in result.problems],
        changed=list(result.changed),
        restart_required=list(result.restart_required),
        reload=(
            build_reload_response(result.reload) if result.reload is not None else None
        ),
        setup_mode_exited=result.setup_mode_exited,
        backup_path=result.backup_path,
    )
