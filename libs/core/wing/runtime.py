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
from wing.config import get_config
from wing.hooks import load_hooks
from wing.request_context import (
    get_request_context,
    reset_request_context,
    set_request_context,
)
from typing import TYPE_CHECKING
from wing.session import Session, SessionManager, SessionReaper
from wing.system import ReloadResult, reload_system as _reload_system
from wing.store import FileSessionStore, MemorySessionStore, SessionStore

if TYPE_CHECKING:
    from wing.provider.registry import ProviderModels
    from wing.session import AgentOverride, AgentTemplateManager


# ============================================================
# WingRuntime
# ============================================================


class WingRuntime:
    """核心 service 层——协调者。

    持有 SessionManager，管理 RequestContext 和路由表。
    将请求路由到 Session/ContextManager，管理事件发射。
    """

    def __init__(self) -> None:
        # 显式安装内置能力（顶层 wing/__init__ 不再有 import 副作用）：
        #   - import wing.tools：装饰器注册内置工具（第一次 tool_registry.resolve
        #     之前必须完成，否则 AgentTemplate.from_config 解析不到任何工具）
        #   - audit.install()：注册 handler 并订阅 EventBus（幂等）
        import wing.tools  # noqa: F401
        from wing.audit import install as install_metrics

        install_metrics()

        load_hooks(get_config().hooks)
        # TODO(future): config 驱动的 backend 选择（sessions.backend / dsn）——
        # SQL 后端（SQLite/PG/Supabase）到来时的扩展点。
        stores: dict[str, SessionStore] = {
            "file": FileSessionStore(get_config().sessions.resolved_path()),
            "memory": MemorySessionStore(),
        }
        self.sm = SessionManager(stores)
        self.reaper = SessionReaper(self.sm)
        """空闲会话逐出器（gateway lifespan 负责 attach/detach）。"""

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

    def ensure_loaded(self, session_id: str) -> Session:
        """取会话；不在内存（被逐出 / 未加载）时从磁盘水合。

        Raises:
            LookupError: 内存与磁盘都没有该会话
        """
        return self.sm.ensure_loaded(session_id)

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
    # Session 逐出（eviction）
    # ============================================================

    def release_session(self, session_id: str) -> tuple[bool, str]:
        """显式逐出会话（忽略空闲时长，不忽略钉住条件）。

        Returns:
            (released, detail)：released=False 表示会话本就不在内存（幂等）。

        Raises:
            LookupError: 内存与磁盘都没有该会话
            RuntimeError: 被钉住（忙碌 / 有待处理输入 / 被订阅 / 非持久后端）
        """
        return self.sm.release_session(session_id)

    async def reap_idle_sessions(self) -> list[str]:
        """扫描一轮并逐出空闲会话（BackgroundScheduler 的 job 入口）。"""
        return await self.reaper.sweep()

    # ============================================================
    # 订阅管理
    # ============================================================

    def subscribe(self, client_id: str, session_id: str) -> None:
        """订阅 session 事件。

        不在内存的会话按需水合（被逐出的会话重新订阅即恢复直播）。

        Raises:
            LookupError: session 不存在
        """
        session = self.sm.ensure_loaded(session_id)
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

    async def compact_session(
        self, session_id: str, instruction: str | None = None
    ) -> tuple[int, int]:
        """压缩 session 上下文。

        instruction 为用户下发的压缩侧重指令（/compact <侧重>），
        透传给 Compactor 附加到压缩 prompt；None 表示使用默认压缩策略。

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
            instruction=instruction,
        )

        self._emit_session_event(
            CompactDoneEvent(
                session_id=session.session_id,
                original_tokens=original,
                compressed_tokens=compressed,
                model=session.agent.model,
            ),
            session=session,
        )
        self._emit_context_stats(session)
        return original, compressed

    async def interrupt_session(
        self, session_id: str, request_id: str | None = None
    ) -> None:
        """中断 session 当前 agent 任务。

        agent.interrupt() 内部 await 旧 worker 完成补提交（流式半截
        reasoning/text 的 partial Message 已落盘），之后 InterruptedEvent
        才落盘+广播——链序保证事件在 partial Message 之后。
        request_id 仅用于日志关联（网关端点生成并透传）。

        Raises:
            LookupError: session 不存在
        """
        session = self._require_session(session_id)
        await session.agent.interrupt(request_id=request_id)
        self._emit_session_event(
            InterruptedEvent(session_id=session.session_id),
            session=session,
        )

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
        agent = session.agent

        draft = cm.rewind(target_uuid)
        self._emit_context_stats(session)

        from wing.event import wire_dump

        turn_started_at = (
            agent.turn_started_at.isoformat() if agent.turn_started_at else None
        )
        self._emit_session_event(
            SyncSessionEvent(
                session_id=session.session_id,
                messages=session.serialize_messages(),
                uncommitted=agent.uncommitted_message(),
                uncommitted_tools=agent.uncommitted_tools(),
                events=[
                    wire_dump(e)
                    for e in cm.get_active_events(
                        pending_ask_ids=agent.pending_ask_ids()
                    )
                ],
                status=session.status,
                turn_started_at=turn_started_at,
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
        provider: str | None = None,
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

        template = None
        if agent is not None:
            template = self.template_manager.get(agent)
            if template is None:
                available = self.template_manager.all_names
                raise LookupError(
                    f"template '{agent}' not found, available: {available}"
                )

        await session.update_state(
            model=model,
            provider_name=provider,
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
        model_emitted = model is not None or agent_switched
        self._emit_session_event(
            SessionStateChangedEvent(
                session_id=session.session_id,
                model=session.agent.model if model_emitted else None,
                model_display_name=session.agent.model_display_name
                if model_emitted
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

    async def list_models(self) -> list["ProviderModels"]:
        """可用模型列表（跨 provider 聚合，按 provider 分组）。

        转发 provider 包 registry（模块级持有所有 provider client；配置了
        静态 models 的 provider 跳过请求）。gateway 路由经此获取，不感知 config。
        """
        from wing.provider.registry import list_all_models

        return await list_all_models()

    async def reload_system(self) -> ReloadResult:
        """热重载全局配置、hooks、prompt commands、provider、skills & rules。

        config 加载失败时立即中止。其余项失败时继续。

        流程本体在 ``wing/system.py``（11 归位：逐字自本方法抽出，步骤顺序与
        失败语义不变）——本方法只做委托，调用点（gateway 路由）不变。
        """
        return await _reload_system(self.sm)

    # ============================================================
    # 内部辅助
    # ============================================================

    def _require_session(self, session_id: str) -> Session:
        """获取 session，不存在则 raise LookupError。"""
        session = self.sm.get_session(session_id)
        if session is None:
            raise LookupError(f"Session not found: {session_id}")
        return session

    def _emit_session_event(
        self, event: WingEvent, session: Session | None = None
    ) -> None:
        """发射 session 级别事件，强制 target=EventTarget(scope="session")。

        persist=true 且 session 给定时先落盘进链（与 AgentEventSink 同一
        持久化语义）；session 为 None 的事件（无会话上下文）只广播。
        request_id 在落盘前从 RequestContext 定型注入——磁盘记录与广播
        帧携带同一关联值（与 AgentEventSink.emit 一致）。
        """
        ctx = get_request_context()
        if ctx.request_id is not None:
            event.request_id = ctx.request_id
        event.target = EventTarget(scope="session")
        if event.persist and session is not None:
            session.context_manager.append_event(event)
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
        """向指定 client 推送 SyncSessionEvent + SessionInitEvent + ContextStatsEvent。

        SyncSession 携带四组重放素材：messages（已提交 Message 投影）、
        uncommitted（单个未提交 assistant Message 投影）、uncommitted_tools
        （未终结 tool 调用的原始 args 片段）、events（活跃链事实事件，按链序）
        ——中途订阅者据此获得与从始至终订阅一致的完整视图，组装顺序为
        messages → uncommitted → uncommitted_tools → events。快照同时是
        **状态**：status（快照时刻的运行状态，前端据此进入 working——内容投影
        为空 ≠ 不在跑，一轮 LLM 调用在飞行时两者都空）与 turn_started_at
        （恢复 working 已耗时）。
        """
        client_target = EventTarget(scope="client", client_ids=[client_id])
        from wing.event import wire_dump

        cm = session.context_manager
        agent = session.agent

        turn_started_at = (
            agent.turn_started_at.isoformat() if agent.turn_started_at else None
        )
        event_bus.emit(
            SyncSessionEvent(
                session_id=session.session_id,
                messages=session.serialize_messages(),
                uncommitted=agent.uncommitted_message(),
                uncommitted_tools=agent.uncommitted_tools(),
                events=[
                    wire_dump(e)
                    for e in cm.get_active_events(
                        pending_ask_ids=agent.pending_ask_ids()
                    )
                ],
                status=session.status,
                turn_started_at=turn_started_at,
                agent=session.to_agent_info(),
                name=session.session_name,
                draft=draft,
                target=client_target,
            )
        )

        event_bus.emit(
            SessionInitEvent(
                session_id=session.session_id,
                tools=[t.effective_llm_name for t in agent.tools],
                model=agent.model,
                permission_mode="bypassPermissions" if agent.yolo else "default",
                cwd=str(agent.cwd) if agent.cwd else "",
                target=client_target,
            )
        )

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
