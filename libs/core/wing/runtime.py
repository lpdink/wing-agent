# wing/runtime.py
"""
wing/runtime.py — WingRuntime：核心最高抽象

组合 SessionManager + EventBus，提供统一入站入口。

设计约束：
  - post() 是唯一入站入口，统一管理 RequestContext（try/finally 确保恢复）
  - Session 生命周期方法（create/resume/fork）语义单一，不涉及路由
  - 订阅管理（subscribe/unsubscribe）封装路由注册 + SyncSession 推送
  - SM 不感知 RequestContext 和路由表（由 Runtime 统一处理）
"""

from __future__ import annotations

from wing.event import (
    ContextStatsEvent,
    EventTarget,
    SessionInfo,
    SessionInitEvent,
    SyncSessionEvent,
)
from wing.event_bus import event_bus
from wing.config import get_config, load_hooks
from wing.request_context import reset_request_context, set_request_context
from typing import TYPE_CHECKING
from wing.session import Session
from wing.session_manager import SessionManager

if TYPE_CHECKING:
    from wing.agent_template import AgentTemplateManager
    from wing.gateway.protocol import AgentOverride


class WingRuntime:
    """核心最高抽象——统一入口。

    持有 SessionManager，管理 RequestContext 和路由表。
    初始化时通过 config 加载用户自定义 hook。
    post() 是唯一入站入口，Gateway 和 TUI 均通过此接口通信。
    """

    def __init__(self) -> None:
        load_hooks(get_config().hooks)
        self.sm = SessionManager()

    @property
    def template_manager(self) -> AgentTemplateManager:
        """Agent 模板管理器。"""
        return self.sm.template_manager

    # ============================================================
    # 入站入口
    # ============================================================

    async def post(
        self,
        content: str,
        request_id: str | None = None,
        session_id: str | None = None,
        client_id: str | None = None,
        silent: bool = False,
    ) -> None:
        """唯一入站入口。设置 RequestContext，try/finally 确保恢复。

        RequestContext 在协程内隔离：
          - request_id：由 caller 指定或 None（EventBus 保留事件原有值）
          - session_id：从参数注入，event emit 时自动填充
          - client_id：注入供下游使用

        如果 session_id 对应 session 不存在，SM._post() 会 log error
        并直接 return——此时 RequestContext 仍然正确恢复。
        """
        token = set_request_context(
            request_id=request_id,
            session_id=session_id,
            client_id=client_id,
        )
        try:
            await self.sm._post(
                content=content,
                request_id=request_id,
                session_id=session_id,
                client_id=client_id,
                silent=silent,
            )
        finally:
            reset_request_context(token)

    # ============================================================
    # Session 生命周期（原子操作，不涉及路由）
    # ============================================================

    def create_session(
        self,
        template_name: str | None = None,
        workspace: str | None = None,
        agent_override: AgentOverride | None = None,
    ) -> Session:
        """创建新 session。session_id 由后端生成。

        不接受 session_id 参数（磁盘恢复用 resume_session）。
        不涉及路由操作（订阅用 subscribe）。

        Args:
            template_name: Agent 模板名称，None 时使用默认模板
            workspace: 工作目录
            agent_override: AgentOverride 参数覆盖（None 字段不覆盖 template 值）
        """
        return self.sm.create_session(
            template_name=template_name,
            workspace=workspace,
            agent_override=agent_override,
        )

    def resume_session(self, session_id: str) -> Session:
        """从磁盘恢复已有 session。

        如果 session 已存在于内存中，直接返回。
        支持 session_id 模糊匹配（前缀/包含）。
        使用默认模板恢复（原始模板信息未持久化）。

        Args:
            session_id: 要恢复的 session ID（支持模糊匹配）

        Raises:
            ValueError: session 不存在
        """
        # 1. 检查内存中是否已存在
        existing = self.sm.get_session(session_id)
        if existing is not None:
            return existing

        # 2. 解析 session_id（支持模糊匹配）
        resolved_id = self.sm.resolve_session_id(session_id)
        if resolved_id is None:
            raise ValueError(f"Session not found: {session_id}")

        # 已在内存中（模糊匹配命中了不同的 key）
        existing = self.sm.get_session(resolved_id)
        if existing is not None:
            return existing

        # 3. 从磁盘加载
        return self.sm.load_session_from_disk(resolved_id)

    def fork_session(
        self,
        source_session_id: str,
        target_uuid: str,
    ) -> tuple[Session, str | None]:
        """从 source_session 的 target_uuid 处分叉出新 session。

        Args:
            source_session_id: 源 session ID
            target_uuid: 分叉点消息 UUID

        Returns:
            (new_session, draft) — draft 为用户未发送的草稿

        Raises:
            ValueError: source session 不存在或 target_uuid 无效
        """
        result = self.sm.fork_session(
            session_id=source_session_id,
            target_uuid=target_uuid,
        )
        if result is None:
            raise ValueError(
                f"Fork failed: source session '{source_session_id}' not found "
                f"or target_uuid '{target_uuid}' invalid"
            )
        return result

    # ============================================================
    # 订阅管理（封装路由表 + SyncSession 推送）
    # ============================================================

    def subscribe(self, client_id: str, session_id: str) -> None:
        """订阅 session 事件。

        内部：
          1. event_bus.route_attach(client_id, session_id)
          2. 推送 SyncSessionEvent 到该 client
          3. 推送 ContextStatsEvent

        Args:
            client_id: 客户端标识
            session_id: 要订阅的 session ID

        Raises:
            ValueError: session 不存在
        """
        session = self.sm.get_session(session_id)
        if session is None:
            raise ValueError(f"Session not found: {session_id}")

        event_bus.route_attach(client_id, session_id)
        self._push_sync(client_id, session)

    def unsubscribe(self, client_id: str, session_id: str) -> None:
        """取消订阅 session 事件。

        Args:
            client_id: 客户端标识
            session_id: 要取消订阅的 session ID
        """
        event_bus.route_detach(client_id, session_id)

    # ============================================================
    # 查询
    # ============================================================

    def list_sessions(self, workspace: str | None = None) -> list[SessionInfo]:
        """列出所有 session（磁盘上的）。

        Args:
            workspace: 可选，过滤特定工作目录的 session
        """
        return self.sm.list_sessions(workspace)

    def get_session_state(self, session_id: str) -> dict | None:
        """获取 session 完整状态。

        Returns:
            包含 session_id, name, template_name, workspace, messages, agent 的 dict，
            或 None（session 不存在）。
        """
        session = self.sm.get_session(session_id)
        if session is None:
            return None

        return {
            "session_id": session.session_id,
            "name": session.session_name,
            "template_name": session.template_name,
            "workspace": session.session_workspace,
            "messages": session.serialize_messages(),
            "agent": session.to_agent_info(),
        }

    # ============================================================
    # 内部辅助
    # ============================================================

    def _push_sync(
        self, client_id: str, session: Session, draft: str | None = None
    ) -> None:
        """向指定 client 推送 SyncSessionEvent + ContextStatsEvent。

        subscribe() 和 fork 后的同步都复用此方法。
        """
        event_bus.emit(
            SyncSessionEvent(
                session_id=session.session_id,
                messages=session.serialize_messages(),
                agent=session.to_agent_info(),
                name=session.session_name,
                draft=draft,
                target=EventTarget(scope="client", client_ids=[client_id]),
            )
        )

        # TODO: SessionInitEvent 与 SyncSessionEvent 存在信息重叠。当前保持独立，
        # 未来考虑统一。
        agent = session.agent
        event_bus.emit(
            SessionInitEvent(
                session_id=session.session_id,
                tools=[t.name for t in agent.tools],
                model=agent.model,
                permission_mode="bypassPermissions" if agent.yolo else "default",
                cwd=agent.state.get("cwd") or "",
                target=EventTarget(scope="client", client_ids=[client_id]),
            )
        )

        # ContextStats
        cm = session.agent.context_manager
        count, tokens = cm.get_context_stats()
        ctx_window = 0
        if cm.compactor:
            ctx_window = cm.compactor.context_window_tokens
        event_bus.emit(
            ContextStatsEvent(
                session_id=session.session_id,
                message_count=count,
                total_tokens=tokens,
                context_window_tokens=ctx_window,
                target=EventTarget(scope="client", client_ids=[client_id]),
            )
        )
