# wing/runtime.py
"""
wing/runtime.py — WingRuntime：核心最高抽象

组合 SessionManager + EventBus，提供统一入站入口。

设计约束：
  - post() 是唯一入站入口，统一管理 RequestContext（try/finally 确保恢复）
  - Session 生命周期方法（create/resume/fork）语义单一，不涉及路由
  - 订阅管理（subscribe/unsubscribe）封装路由注册 + SyncSession 推送
  - SM 不感知 RequestContext 和路由表（由 Runtime 统一处理）
  - 所有业务逻辑（compact/interrupt/rewind/reload/update）封装为 Runtime 方法
  - Route handler 只做参数验证和 HTTP 响应构造
"""

from __future__ import annotations

import uuid

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
from wing.common.logger import log
from wing.request_context import reset_request_context, set_request_context
from typing import TYPE_CHECKING
from wing.schema import Message
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
    所有 session 操作和系统操作的实现都在这里——route handler 只做
    参数验证 → 调 Runtime 方法 → 构造 HTTP 响应。
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
        """
        return self.sm.create_session(
            template_name=template_name,
            workspace=workspace,
            agent_override=agent_override,
        )

    def resume_session(self, session_id: str) -> Session:
        """从磁盘恢复已有 session。支持模糊匹配。"""
        existing = self.sm.get_session(session_id)
        if existing is not None:
            return existing

        resolved_id = self.sm.resolve_session_id(session_id)
        if resolved_id is None:
            raise ValueError(f"Session not found: {session_id}")

        existing = self.sm.get_session(resolved_id)
        if existing is not None:
            return existing

        return self.sm.load_session_from_disk(resolved_id)

    def fork_session(
        self,
        source_session_id: str,
        target_uuid: str,
    ) -> tuple[Session, str | None]:
        """从 source_session 的 target_uuid 处分叉出新 session。"""
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
        """订阅 session 事件。"""
        session = self.sm.get_session(session_id)
        if session is None:
            raise ValueError(f"Session not found: {session_id}")

        event_bus.route_attach(client_id, session_id)
        self._push_sync(client_id, session)

    def unsubscribe(self, client_id: str, session_id: str) -> None:
        """取消订阅 session 事件。"""
        event_bus.route_detach(client_id, session_id)

    # ============================================================
    # 查询
    # ============================================================

    def list_sessions(self, workspace: str | None = None) -> list[SessionInfo]:
        """列出所有 session（磁盘上的）。"""
        return self.sm.list_sessions(workspace)

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
            "messages": session.serialize_messages(),
            "agent": session.to_agent_info(),
        }

    def get_session(self, session_id: str) -> Session | None:
        """获取 session 实例。"""
        return self.sm.get_session(session_id)

    # ============================================================
    # Session 操作（原魔术命令迁移）
    # ============================================================

    async def compact_session(self, session_id: str) -> tuple[int, int]:
        """压缩 session 上下文。

        Returns:
            (original_tokens, compressed_tokens)

        Raises:
            ValueError: session 不存在
            RuntimeError: 未配置 compactor 或压缩失败
        """
        session = self._require_session(session_id)
        agent = session.agent
        cm = agent.context_manager
        if not cm.compactor:
            raise RuntimeError("compactor not configured")

        cm._discard_pending_compact()
        msgs = list(cm._messages)
        full_context = [cm.system_prompt] + msgs
        compacted = await cm.compactor.do_compact(
            full_context,
            agent.model,
            agent.model_provider,
            tools=agent.tools,
        )
        last_compressed_uuid = msgs[-1].uuid if msgs else None
        compact_node = Message(
            role="assistant",
            content=compacted.content,
            parent_uuid=None,
            unzip_last_uuid=last_compressed_uuid,
        )
        compact_node.uuid = uuid.uuid4().hex

        cm._messages.append_detached(compact_node)
        cm._messages.set_tip(compact_node.uuid)

        self._emit_session_event(
            CompactDoneEvent(
                session_id=session.session_id,
                original_tokens=compacted.usage.prompt_tokens,
                compressed_tokens=compacted.usage.completion_tokens,
                model=agent.model,
            )
        )
        self._emit_context_stats(session)

        return compacted.usage.prompt_tokens, compacted.usage.completion_tokens

    def interrupt_session(self, session_id: str) -> None:
        """中断 session 当前 agent 任务。

        Raises:
            ValueError: session 不存在
        """
        session = self._require_session(session_id)
        session.agent.interrupt()
        self._emit_session_event(InterruptedEvent(session_id=session.session_id))

    def rewind_session(self, session_id: str, target_uuid: str) -> str | None:
        """回退 session 到指定消息节点。

        Returns:
            draft 文本（用户未发送的草稿），或 None

        Raises:
            ValueError: session 不存在或 target_uuid 无效
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
        model: str | None = None,
        agent: str | None = None,
        title: str | None = None,
        thinking: bool | None = None,
        reasoning_effort: str | None = None,
        yolo: bool | None = None,
    ) -> None:
        """统一更新 session 状态。

        按 agent → model → title → thinking → reasoning_effort → yolo 顺序执行。

        Raises:
            ValueError: session 不存在或 template 不存在
        """
        session = self._require_session(session_id)

        if agent is not None:
            template = self.template_manager.get(agent)
            if template is None:
                available = self.template_manager.all_names
                raise ValueError(
                    f"template '{agent}' not found, available: {available}"
                )
            await session.switch_template(template)

        if model is not None:
            session.agent.model = model

        if title is not None:
            session.set_title(title)

        if thinking is not None:
            session.agent.model_provider.set_thinking(thinking)

        if reasoning_effort is not None:
            session.agent.set_reasoning_effort(reasoning_effort)

        if yolo is not None:
            session.agent.set_yolo(yolo)

        # emit SessionStateChangedEvent — agent 切换会重置 thinking/reasoning_effort/yolo
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

    def reload_system(self) -> tuple[bool, list[dict]]:
        """热重载全局配置。

        Returns:
            (all_ok, results) — results 是 [{"name": ..., "ok": ..., "detail": ...}, ...]
        """
        from wing.config import load_config, load_hooks
        from wing.hook_registry import hooks
        from wing.magic_command.prompt_commands import register_prompt_commands
        from wing.magic_command.registry import magic_registry

        results: list[dict] = []
        total = 5
        success = 0

        # 1. Reload config
        try:
            config = load_config(reload=True)
            results.append({"name": "config.yaml", "ok": True, "detail": None})
            success += 1
        except Exception as e:
            results.append({"name": "config.yaml", "ok": False, "detail": str(e)})
            return False, results

        # 2. Reload hooks
        try:
            hooks.clear()
            load_hooks(config.hooks)
            results.append({"name": "hooks", "ok": True, "detail": None})
            success += 1
        except Exception as e:
            results.append({"name": "hooks", "ok": False, "detail": str(e)})

        # 3. Reload prompt commands
        try:
            magic_registry.remove_by_source("prompt")
            register_prompt_commands(config.commands.paths)
            results.append({"name": "prompt commands", "ok": True, "detail": None})
            success += 1
        except Exception as e:
            results.append({"name": "prompt commands", "ok": False, "detail": str(e)})

        # 4. Reload OpenAI provider — 所有 session 的 provider
        try:
            all_changes: list[str] = []
            for session in self.sm.iter_sessions():
                changes = session.agent.model_provider.reload()
                all_changes.extend(changes)
            detail = ", ".join(all_changes) if all_changes else "unchanged"
            results.append({"name": "provider", "ok": True, "detail": detail})
            success += 1
        except Exception as e:
            results.append({"name": "provider", "ok": False, "detail": str(e)})

        # 5. Reload skills & rules for all active sessions
        try:
            for session in self.sm.iter_sessions():
                session.agent.context_manager.reload_skills_and_rules()
            results.append({"name": "skills & rules", "ok": True, "detail": None})
            success += 1
        except Exception as e:
            results.append({"name": "skills & rules", "ok": False, "detail": str(e)})

        return success == total, results

    # ============================================================
    # 内部辅助
    # ============================================================

    def _require_session(self, session_id: str) -> Session:
        """获取 session，不存在则 raise ValueError。"""
        session = self.sm.get_session(session_id)
        if session is None:
            raise ValueError(f"Session not found: {session_id}")
        return session

    def _emit_session_event(self, event: WingEvent) -> None:
        """emit session 级别事件，自动设置 target=EventTarget(scope="session")。"""
        if event.target is None:
            event.target = EventTarget(scope="session")
        event_bus.emit(event)

    def _emit_context_stats(self, session: Session) -> None:
        """emit ContextStatsEvent — compact 和 rewind 改变上下文后共用。"""
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
        """向指定 client 推送 SyncSessionEvent + ContextStatsEvent。"""
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
