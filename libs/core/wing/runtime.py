# wing/runtime.py
"""
wing/runtime.py — WingRuntime：service 层协调者

组合 SessionManager + EventBus，提供统一入站入口。

架构定位：
  Runtime 是协调者，不是实现者。它将用户请求路由到正确的组件，
  并管理事件的发射——但具体的业务逻辑由 Session、ContextManager
  等组件自己实现。

设计约束：
  - post() 是唯一入站入口，统一管理 RequestContext
  - Session 生命周期方法语义单一
  - 订阅管理封装路由注册 + SyncSession 推送
  - 所有事件通过 _emit_session_event() 发射，保证 scope="session"
  - 异常层次：LookupError（not found）/ ValueError（bad input）/
    RuntimeError（bad state），route 按类型映射 HTTP status
"""

from __future__ import annotations

from dataclasses import dataclass, field

from wing.event import (
    BranchTargetInfo,
    BranchTargetsEvent,
    CompactDoneEvent,
    ContextStatsEvent,
    EventTarget,
    InterruptedEvent,
    SessionInfo,
    SessionInitEvent,
    SessionStateChangedEvent,
    SyncSessionEvent,
    WingEvent,
)
from wing.event_bus import event_bus
from wing.config import get_config, load_hooks
from wing.request_context import reset_request_context, set_request_context
from typing import TYPE_CHECKING
from wing.session import Session
from wing.session_manager import SessionManager
from wing.store import FileSessionStore, MemorySessionStore, SessionStore

if TYPE_CHECKING:
    from wing.agent_template import AgentTemplate, AgentTemplateManager
    from wing.gateway.protocol import AgentOverride


# ============================================================
# 公共数据类
# ============================================================


@dataclass
class ReloadResultItem:
    """reload_system 中单项重载的结果。"""

    name: str
    ok: bool
    detail: str | None = None


@dataclass
class ReloadResult:
    """reload_system 的完整结果。"""

    ok: bool
    items: list[ReloadResultItem] = field(default_factory=list)


# ============================================================
# WingRuntime
# ============================================================


class WingRuntime:
    """核心 service 层——协调者。

    持有 SessionManager，管理 RequestContext 和路由表。
    将请求路由到 Session/ContextManager，管理事件发射。
    """

    def __init__(self) -> None:
        load_hooks(get_config().hooks)
        # TODO(future): config 驱动的 backend 选择（sessions.backend / dsn）——
        # SQL 后端（SQLite/PG/Supabase）到来时的扩展点。
        stores: dict[str, SessionStore] = {
            "file": FileSessionStore(get_config().sessions.resolved_path()),
            "memory": MemorySessionStore(),
        }
        self.sm = SessionManager(stores)

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
        tool_call_id: str | None = None,
    ) -> None:
        """唯一入站入口。设置 RequestContext，try/finally 确保恢复。"""
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
                tool_call_id=tool_call_id,
            )
        finally:
            reset_request_context(token)

    # ============================================================
    # Session 生命周期
    # ============================================================

    def create_session(
        self,
        template_name: str | None = None,
        workspace: str | None = None,
        agent_override: AgentOverride | None = None,
        backend: str | None = None,
    ) -> Session:
        """创建新 session。

        Args:
            backend: 存储后端（file/memory），None 使用默认后端（file）。

        Raises:
            ValueError: 模板不存在或 backend 未知
        """
        return self.sm.create_session(
            template_name=template_name,
            workspace=workspace,
            agent_override=agent_override,
            backend=backend,
        )

    def resume_session(self, session_id: str) -> Session:
        """从磁盘恢复已有 session。支持模糊匹配。

        Raises:
            LookupError: session 不存在
        """
        return self.sm.resume_session(session_id)

    def fork_session(
        self,
        source_session_id: str,
        target_uuid: str,
    ) -> tuple[Session, str | None]:
        """从 source_session 的 target_uuid 处分叉出新 session。

        Raises:
            LookupError: session 或 uuid 不存在
        """
        result = self.sm.fork_session(
            session_id=source_session_id,
            target_uuid=target_uuid,
        )
        if result is None:
            raise LookupError(
                f"Fork failed: source session '{source_session_id}' not found "
                f"or target_uuid '{target_uuid}' invalid"
            )
        return result

    # ============================================================
    # 订阅管理
    # ============================================================

    def subscribe(self, client_id: str, session_id: str) -> None:
        """订阅 session 事件。

        Raises:
            LookupError: session 不存在
        """
        session = self._require_session(session_id)
        event_bus.route_attach(client_id, session_id)
        self._push_sync(client_id, session)

    def unsubscribe(self, client_id: str, session_id: str) -> None:
        """取消订阅 session 事件。"""
        event_bus.route_detach(client_id, session_id)

    # ============================================================
    # 查询
    # ============================================================

    def list_sessions(self) -> list[SessionInfo]:
        """列出所有 session（磁盘上的），按时间降序，携带运行时状态。"""
        return self.sm.list_sessions()

    def get_session_state(self, session_id: str) -> dict | None:
        """获取 session 完整状态。返回 dict 或 None。"""
        session = self.sm.get_session(session_id)
        if session is None:
            return None

        return {
            "session_id": session.session_id,
            "name": session.session_name,
            "template_name": session.template_name,
            "workspace": session.session_workspace,
            "status": session.status,
            "messages": session.serialize_messages(),
            "agent": session.to_agent_info(),
        }

    def get_session(self, session_id: str) -> Session | None:
        """获取 session 实例。"""
        return self.sm.get_session(session_id)

    # ============================================================
    # Session 操作（协调：委托给 Session/CM，自己只管事件）
    # ============================================================

    async def compact_session(self, session_id: str) -> tuple[int, int]:
        """压缩 session 上下文。

        委托给 ContextManager.do_manual_compact()，发射事件。

        Returns:
            (original_tokens, compressed_tokens)

        Raises:
            LookupError: session 不存在
            RuntimeError: 未配置 compactor 或压缩失败
        """
        session = self._require_session(session_id)
        original, compressed = await session.agent.context_manager.do_manual_compact(
            model=session.agent.model,
            model_provider=session.agent.model_provider,
            current_tools=lambda: session.agent.tools,
        )

        self._emit_session_event(
            CompactDoneEvent(
                session_id=session.session_id,
                original_tokens=original,
                compressed_tokens=compressed,
                model=session.agent.model,
            )
        )
        self._emit_context_stats(session)
        return original, compressed

    def interrupt_session(self, session_id: str) -> None:
        """中断 session 当前 agent 任务。

        Raises:
            LookupError: session 不存在
        """
        session = self._require_session(session_id)
        session.agent.interrupt()
        self._emit_session_event(InterruptedEvent(session_id=session.session_id))

    def rewind_session(self, session_id: str, target_uuid: str) -> str | None:
        """回退 session 到指定消息节点。

        委托给 ContextManager.rewind()，发射事件。

        Returns:
            draft 文本，或 None

        Raises:
            LookupError: session 不存在
            ValueError: target_uuid 无效
        """
        session = self._require_session(session_id)
        cm = session.agent.context_manager

        draft = cm.rewind(target_uuid)
        self._emit_context_stats(session)

        self._emit_session_event(
            SyncSessionEvent(
                session_id=session.session_id,
                messages=[msg.model_dump() for msg in cm.get_context_window()],
                agent=None,
                draft=draft,
            )
        )

        targets = cm.get_branch_targets()
        self._emit_session_event(
            BranchTargetsEvent(
                session_id=session.session_id,
                targets=[BranchTargetInfo(**t) for t in targets],
            )
        )
        return draft

    async def update_session(
        self,
        session_id: str,
        *,
        model: str | None = None,
        agent: str | None = None,
        title: str | None = None,
        thinking: bool | None = None,
        reasoning_effort: str | None = None,
        yolo: bool | None = None,
        workspace: str | None = None,
        tools: list[str] | None = None,
    ) -> None:
        """统一更新 session 状态。

        委托给 Session.update_state()，计算 event 字段后发射事件。

        Raises:
            LookupError: session 或 template 不存在
            ValueError: workspace 路径不合法 / 工具引用无法解析
        """
        session = self._require_session(session_id)

        # 解析 template（如有）
        template = None
        if agent is not None:
            template = self.template_manager.get(agent)
            if template is None:
                available = self.template_manager.all_names
                raise LookupError(
                    f"template '{agent}' not found, available: {available}"
                )

        # 委托给 Session 执行状态变更
        await session.update_state(
            model=model,
            template=template,
            title=title,
            thinking=thinking,
            reasoning_effort=reasoning_effort,
            yolo=yolo,
            workspace=workspace,
            tools=tools,
        )

        # 计算 event 字段——agent 切换会重置 thinking/reasoning_effort/yolo
        agent_switched = agent is not None
        self._emit_session_event(
            SessionStateChangedEvent(
                session_id=session.session_id,
                model=session.agent.model
                if model is not None or agent_switched
                else None,
                thinking=session.agent.model_provider.thinking
                if thinking is not None or agent_switched
                else None,
                reasoning_effort=session.agent.model_provider.reasoning_effort
                if reasoning_effort is not None or agent_switched
                else None,
                yolo=session.agent.yolo if yolo is not None or agent_switched else None,
                title=session.session_name if title is not None else None,
                agent=session.template_name if agent_switched else None,
            )
        )

    # ============================================================
    # 系统操作
    # ============================================================

    def reload_system(self) -> ReloadResult:
        """热重载全局配置、hooks、prompt commands、provider、skills & rules。

        config 加载失败时立即中止。其余项失败时继续。
        """
        from wing.config import load_config, load_hooks
        from wing.hook_registry import hooks
        from wing.magic_command.prompt_commands import register_prompt_commands
        from wing.magic_command.registry import magic_registry

        items: list[ReloadResultItem] = []

        # 1. Reload config
        try:
            config = load_config(reload=True)
            items.append(ReloadResultItem(name="config.yaml", ok=True))
        except Exception as e:
            items.append(ReloadResultItem(name="config.yaml", ok=False, detail=str(e)))
            return ReloadResult(ok=False, items=items)

        # 2. Reload hooks
        try:
            hooks.clear()
            load_hooks(config.hooks)
            items.append(ReloadResultItem(name="hooks", ok=True))
        except Exception as e:
            items.append(ReloadResultItem(name="hooks", ok=False, detail=str(e)))

        # 3. Reload prompt commands
        try:
            magic_registry.remove_by_source("prompt")
            register_prompt_commands(config.commands.paths)
            items.append(ReloadResultItem(name="prompt commands", ok=True))
        except Exception as e:
            items.append(
                ReloadResultItem(name="prompt commands", ok=False, detail=str(e))
            )

        # 4. Reload OpenAI provider — 所有 session
        try:
            all_changes: list[str] = []
            for session in self.sm.iter_sessions():
                changes = session.agent.model_provider.reload()
                all_changes.extend(changes)
            detail = ", ".join(all_changes) if all_changes else "unchanged"
            items.append(ReloadResultItem(name="provider", ok=True, detail=detail))
        except Exception as e:
            items.append(ReloadResultItem(name="provider", ok=False, detail=str(e)))

        # 5. Reload skills & rules for all active sessions
        try:
            for session in self.sm.iter_sessions():
                session.agent.context_manager.reload_skills_and_rules()
            items.append(ReloadResultItem(name="skills & rules", ok=True))
        except Exception as e:
            items.append(
                ReloadResultItem(name="skills & rules", ok=False, detail=str(e))
            )

        all_ok = all(item.ok for item in items)
        return ReloadResult(ok=all_ok, items=items)

    # ============================================================
    # 内部辅助
    # ============================================================

    def _require_session(self, session_id: str) -> Session:
        """获取 session，不存在则 raise LookupError。"""
        session = self.sm.get_session(session_id)
        if session is None:
            raise LookupError(f"Session not found: {session_id}")
        return session

    def _emit_session_event(self, event: WingEvent) -> None:
        """发射 session 级别事件，强制 target=EventTarget(scope="session")。"""
        event.target = EventTarget(scope="session")
        event_bus.emit(event)

    def _emit_context_stats(self, session: Session) -> None:
        """发射 ContextStatsEvent。"""
        cm = session.agent.context_manager
        count, tokens = cm.get_context_stats()
        ctx_window = 0
        if cm.compactor:
            ctx_window = cm.compactor.context_window_tokens
        self._emit_session_event(
            ContextStatsEvent(
                session_id=session.session_id,
                message_count=count,
                total_tokens=tokens,
                context_window_tokens=ctx_window,
            )
        )

    def _push_sync(
        self, client_id: str, session: Session, draft: str | None = None
    ) -> None:
        """向指定 client 推送 SyncSessionEvent + SessionInitEvent + ContextStatsEvent。"""
        client_target = EventTarget(scope="client", client_ids=[client_id])

        event_bus.emit(
            SyncSessionEvent(
                session_id=session.session_id,
                messages=session.serialize_messages(),
                agent=session.to_agent_info(),
                name=session.session_name,
                draft=draft,
                target=client_target,
            )
        )

        agent = session.agent
        event_bus.emit(
            SessionInitEvent(
                session_id=session.session_id,
                tools=[t.effective_llm_name for t in agent.tools],
                model=agent.model,
                permission_mode="bypassPermissions" if agent.yolo else "default",
                cwd=agent.state.get("cwd") or "",
                target=client_target,
            )
        )

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
                target=client_target,
            )
        )
