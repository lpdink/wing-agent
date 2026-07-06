# wing/runtime.py
"""
wing/runtime.py — WingRuntime：核心最高抽象

组合 SessionManager + EventBus，提供统一入站入口。

设计约束：
  - post() 是唯一入站入口，统一管理 RequestContext（try/finally 确保恢复）
  - 所有 create/fork/switch 统一管理 route_attach
  - SM 不感知 RequestContext 和路由表（由 Runtime 统一处理）
"""

from __future__ import annotations

from wing.event_bus import event_bus
from wing.config import get_config, load_hooks
from wing.request_context import reset_request_context, set_request_context
from wing.session import Session
from wing.session_manager import SessionManager


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
    def template_manager(self):
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
    # Session 生命周期
    # ============================================================

    def create_session(
        self,
        template_name: str | None = None,
        session_id: str | None = None,
        client_id: str | None = None,
        workspace: str | None = None,
    ) -> Session:
        """创建新 session。

        Args:
            template_name: Agent 模板名称，None 时使用默认模板
            session_id: 指定 session_id（磁盘恢复场景）
            client_id: 客户端标识，有值时自动 route_attach
            workspace: 工作目录
        """
        session = self.sm.create_session(
            template_name=template_name,
            session_id=session_id,
            workspace=workspace,
        )
        if client_id is not None:
            event_bus.route_attach(client_id, session.session_id)
        return session

    def fork_session(
        self,
        session_id: str,
        target_uuid: str,
        client_id: str | None = None,
    ) -> tuple[Session, str | None] | None:
        """从指定消息分叉新 session。"""
        result = self.sm.fork_session(
            session_id=session_id,
            target_uuid=target_uuid,
        )
        if result is not None and client_id is not None:
            new_session, _ = result
            event_bus.route_attach(client_id, new_session.session_id)
        return result

    def switch_session(
        self,
        session_id: str,
        target_session_id: str,
        client_id: str | None = None,
    ) -> Session | None:
        """切换到已有 session。"""
        target = self.sm.switch_session(
            session_id=session_id,
            target_session_id=target_session_id,
        )
        if target is not None and client_id is not None:
            event_bus.route_attach(client_id, target.session_id)
        return target
