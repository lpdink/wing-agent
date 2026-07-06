# wing/session_manager.py
"""
wing/session_manager.py — SessionManager

管理 session 生命周期、消息路由、魔术命令分发。

核心设计约束：
  - SessionManager 只处理跨 session 行为（create、switch、fork、列表、id 模糊匹配）
  - 单 session 内部逻辑（metadata 管理、title 设置）在 Session 中
  - WingAgent 不知道 SessionManager 的存在——它通过 EventBus 投递事件。
  - EventBus 不知道 WingAgent 的存在——它只是路由管道。
  - 路由表由 WingRuntime 统一管理，SM 不感知 client_id 和 contextvars。
"""

from __future__ import annotations

import json
import os
import uuid
from datetime import datetime
from pathlib import Path
from typing import Any

from wing.agent_template import AgentTemplate, AgentTemplateManager
from wing.common.logger import log
from wing.common.tracked_list import TrackedList
from wing.common.utils import generate_session_id
from wing.config import get_config
from wing.hook_registry import hooks
from wing.event import (
    AgentInfo,
    BranchTargetInfo,
    BranchTargetsEvent,
    ContextStatsEvent,
    DeliveredEvent,
    EventTarget,
    NewSessionEvent,
    SessionInfo,
    SessionListEvent,
    SyncSessionEvent,
    SystemEvent,
    SystemInfoEvent,
    ModelSwitchedEvent,
)
from wing.event.query_response import AgentListEvent
from wing.event_bus import event_bus
from wing.magic_command.registry import magic_registry
from wing.schema import Message
from wing.session import Session


class SessionManager:
    """管理 session 生命周期、消息路由、魔术命令分发。

    每个 Session 有独立的 WingAgent。
    事件通过全局 EventBus 投递。

    内部方法（_ 开头）：
      - _post：消息路由入口，由 WingRuntime 调用
      - _dispatch_session_command：拦截 /new, /fork, /session, /ss, /title, /agents
      - _dispatch_magic_command：路由到 magic_registry

    外部方法：
      - create_session：创建新 session
      - fork_session：从指定消息分叉
      - switch_session：切换到已有 session

    路由表由 WingRuntime 统一管理，SM 不感知 client_id 和 contextvars。
    """

    # SM 直接拦截的命令名（不再走 magic_registry）
    _SESSION_COMMANDS = {"new", "fork", "session", "ss", "title", "info", "agents"}

    def __init__(self, sessions_path: Path | None = None) -> None:
        self._sessions: dict[str, Session] = {}
        self._sessions_path = sessions_path or get_config().sessions.resolved_path()

        # 从 config 创建 AgentTemplateManager
        config = get_config()
        self._template_manager = AgentTemplateManager(config.agents)

    @property
    def template_manager(self) -> AgentTemplateManager:
        """Agent 模板管理器。"""
        return self._template_manager

    # ============================================================
    # 外部方法：session 生命周期
    # ============================================================

    def get_session(self, session_id: str) -> Session | None:
        return self._sessions.get(session_id)

    def _generate_session_id(self) -> str:
        """生成唯一 session id（委托 common.utils.generate_session_id）。"""
        return generate_session_id()

    def create_session(
        self,
        template_name: str | None = None,
        session_id: str | None = None,
        workspace: str | None = None,
    ) -> Session:
        """创建新 session。

        Args:
            template_name: Agent 模板名称，None 时使用默认模板
            session_id: 指定 session_id（磁盘恢复场景），None 时自动生成
            workspace: 工作目录
        """
        # 查询模板
        if template_name is not None:
            template = self._template_manager.get(template_name)
            if template is None:
                raise ValueError(
                    f"Agent 模板 '{template_name}' 不存在。"
                    f"可用模板: {self._template_manager.all_names}"
                )
        else:
            template = self._template_manager.default

        # 生成或使用传入的 session_id
        sid = session_id if session_id is not None else self._generate_session_id()

        # 创建 TrackedList
        messages: TrackedList[Message] = (
            TrackedList.load(self._sessions_path / sid, Message)
            if session_id is not None
            else TrackedList(self._sessions_path / sid)
        )

        session = Session.from_template(
            template=template,
            session_id=sid,
            messages=messages,
            workspace=workspace,
        )

        self._sessions[sid] = session

        # 触发 before_session_start hook
        hooks.invoke("before_session_start", session)

        log.info(f"Session created: {sid} (template={template.name})")
        return session

    # ── resolve / import ──────────────────────────

    def _resolve_session(self, session_id: str) -> Path | None:
        """解析 session id（支持模糊匹配）。"""
        if not self._sessions_path.exists():
            return None

        if "*" in session_id:
            matches = list(self._sessions_path.glob(session_id))
            return matches[0] if len(matches) == 1 else None

        # 前缀匹配
        matches = list(self._sessions_path.glob(f"{session_id}*"))
        if len(matches) == 1:
            return matches[0]

        # 包含匹配
        matches = list(self._sessions_path.glob(f"*{session_id}*"))
        return matches[0] if len(matches) == 1 else None

    def resolve_session_id(self, session_id: str) -> str | None:
        """解析 session id（支持模糊匹配），返回完全匹配的 session_id。"""
        matched_path = self._resolve_session(session_id)
        if matched_path is None:
            return None
        return matched_path.name

    def import_messages(
        self,
        messages: list[Message],
        source_session_id: str | None = None,
    ) -> str:
        """将消息列表导入新 session，返回新 session_id。"""
        new_session_id = self._generate_session_id()
        new_session_path = self._sessions_path / new_session_id

        new_tl = TrackedList(new_session_path)
        new_tl._type = Message
        # 确保目录存在——即使 messages 为空，后续 metadata 写入也需要
        new_session_path.mkdir(parents=True, exist_ok=True)

        if messages:
            # 建立 UUID 映射：旧 uuid → 新 uuid，保留原始拓扑
            uuid_map: dict[str, str] = {}
            for msg in messages:
                if msg.uuid:
                    uuid_map[msg.uuid] = str(uuid.uuid4())

            # 替换 uuid、parent_uuid、unzip_last_uuid 引用
            for msg in messages:
                if msg.uuid:
                    msg.uuid = uuid_map.get(msg.uuid, str(uuid.uuid4()))
                if msg.parent_uuid:
                    msg.parent_uuid = uuid_map.get(msg.parent_uuid)
                if msg.unzip_last_uuid:
                    msg.unzip_last_uuid = uuid_map.get(msg.unzip_last_uuid)

            # 使用 extend_detached 一次性写入，避免 _ensure_type 自动填充 parent_uuid
            new_tl.extend_detached(messages)

        # 写入 metadata.json 记录 fork 来源
        if source_session_id:
            metadata = {"forked_from": source_session_id}
            metadata_path = new_session_path / "metadata.json"
            with open(metadata_path, "w", encoding="utf-8") as f:
                json.dump(metadata, f, ensure_ascii=False)
                f.flush()
                os.fsync(f.fileno())

        return new_session_id

    def fork_session(
        self,
        session_id: str,
        target_uuid: str,
    ) -> tuple[Session, str | None] | None:
        """从指定 session 的 target_uuid 处 fork 出新 session。"""
        source = self._sessions.get(session_id)
        if source is None:
            return None

        cm = source.agent.context_manager
        try:
            subchain, draft = cm.extract_subchain(target_uuid)
        except ValueError:
            log.warning(f"fork_session: uuid {target_uuid} not found")
            return None

        # import messages
        new_session_id = self.import_messages(subchain, source_session_id=session_id)

        # 从旧 agent 抽取模板
        template = AgentTemplate.from_agent(source.agent, name=source.template_name)

        # 用 from_template 创建新 session
        messages: TrackedList[Message] = TrackedList.load(
            self._sessions_path / new_session_id, Message
        )
        new_session = Session.from_template(
            template=template,
            session_id=new_session_id,
            messages=messages,
            workspace=source.session_workspace,
        )
        self._sessions[new_session_id] = new_session

        return new_session, draft

    def switch_session(
        self,
        session_id: str,
        target_session_id: str,
    ) -> Session | None:
        """切换到已有的 target_session_id。"""
        source = self._sessions.get(session_id)
        if source is None:
            return None

        # 解析 session_id
        resolved_id = self.resolve_session_id(target_session_id)
        if resolved_id is None:
            return None

        # 如果目标 session 已在 _sessions 中，直接返回
        target = self._sessions.get(resolved_id)
        if target is not None:
            return target

        # 目标 session 不在 _sessions 中，需要从磁盘恢复
        # 从旧 agent 抽取模板
        template = AgentTemplate.from_agent(source.agent, name=source.template_name)

        # 加载消息
        messages: TrackedList[Message] = TrackedList.load(
            self._sessions_path / resolved_id, Message
        )

        new_session = Session.from_template(
            template=template,
            session_id=resolved_id,
            messages=messages,
            workspace=source.session_workspace,
        )
        self._sessions[resolved_id] = new_session

        return new_session

    # ============================================================
    # 外部方法：消息路由入口
    # ============================================================

    async def _post(
        self,
        content: str,
        request_id: str | None = None,
        session_id: str | None = None,
        client_id: str | None = None,
        silent: bool = False,
    ) -> None:
        """路由消息到指定 session。（内部方法，由 WingRuntime 调用）

        Contextvars 由 WingRuntime.post() 统一管理，此方法不设置/恢复。

        - / 开头 → 先拦截 session 命令（/new, /fork, /session, /ss, /title）
          → 然后交给 magic_registry
          → 无法匹配时，路由给 agent 视为用户一般输入
        - 否则 → session.post() → agent.post()
        """
        assert session_id is not None, "session_id is required by WingRuntime"
        session = self._sessions.get(session_id)
        if session is None:
            log.error(f"Session not found: {session_id}")
            return

        # DeliveredEvent
        if not silent:
            event_bus.emit(
                DeliveredEvent(
                    session_id=session.session_id,
                    target=EventTarget(scope="session"),
                )
            )

        # 魔术命令路由
        if content.startswith("/"):
            # 先拦截 session 命令
            result, new_sid = await self._dispatch_session_command(
                session, content, client_id
            )
            if result is not None:
                if not silent:
                    event_bus.emit(
                        SystemEvent(
                            session_id=new_sid or session.session_id,
                            content=result,
                            target=EventTarget(scope="session"),
                        )
                    )
                return

            # 交给 magic_registry
            result = await self._dispatch_magic_command(session, content)
            if result is None:
                # 命令不匹配，交给 agent 处理
                await session.post(content, request_id=request_id)
                return
            if result is not None and not silent:
                event_bus.emit(
                    SystemEvent(
                        session_id=session.session_id,
                        content=result,
                        target=EventTarget(scope="session"),
                    )
                )
            return

        # 非 / 开头：通过 Session.post 投递
        log.info(
            f"SM._post: routing '{content[:50]}' to session.post (session={session_id})"
        )
        await session.post(content, request_id=request_id)

    # ============================================================
    # 内部方法：session 命令分发
    # ============================================================

    async def _dispatch_session_command(
        self,
        session: Session,
        content: str,
        client_id: str | None,
    ) -> tuple[str | None, str | None]:
        """拦截 session 命令：/new, /fork, /session, /ss, /title,/agents。

        Returns:
            (result_text, new_session_id): result_text 为 None 表示命令不匹配。
            new_session_id 非 None 表示产生了新 session（用于 SystemEvent 路由）。
        """
        parts = content.lstrip("/").split(maxsplit=1)
        cmd_name = parts[0]
        args = parts[1] if len(parts) > 1 else ""

        if cmd_name not in self._SESSION_COMMANDS:
            return None, None

        if cmd_name == "new":
            return await self._cmd_new(session, args, client_id)
        elif cmd_name in ("session", "ss"):
            return await self._cmd_session(session, args, client_id)
        elif cmd_name == "fork":
            return await self._cmd_fork(session, args, client_id)
        elif cmd_name == "title":
            return await self._cmd_title(session, args)
        elif cmd_name == "info":
            return self._cmd_info(session)
        elif cmd_name == "agents":
            return await self._cmd_agents(session, args)

        return None, None

    async def _cmd_new(
        self,
        session: Session,
        args: str,
        client_id: str | None,
    ) -> tuple[str, str]:
        """处理 /new [name] 命令。"""
        name = args.strip() if args else None
        original_session_id = session.session_id

        new_session = self.create_session(workspace=session.session_workspace)
        new_session_id = new_session.session_id

        event_bus.emit(
            NewSessionEvent(
                session_id=original_session_id,
                new_session_id=new_session_id,
                agent=_build_agent_info(new_session.agent),
                name=name,
                target=EventTarget(scope="session"),
            )
        )
        _emit_context_stats(new_session.agent)
        if name:
            return (
                f"✅ 新 session 已创建: {new_session_id} (名称: {name})",
                new_session_id,
            )
        return f"✅ 新 session 已创建: {new_session_id}", new_session_id

    async def _cmd_session(
        self,
        session: Session,
        args: str,
        client_id: str | None,
    ) -> tuple[str, str | None]:
        """处理 /session [uuid] 或 /ss [uuid] 命令。"""
        if not args:
            sessions = _list_sessions(self._sessions_path, session.session_workspace)
            log.debug(
                f"SM._cmd_session: emitting SessionListEvent with {len(sessions)} sessions"
            )
            event_bus.emit(
                SessionListEvent(
                    session_id=session.session_id,
                    sessions=sessions,
                )
            )
            if not sessions:
                return "暂无 session", None
            lines = ["📋 Session 列表:"]
            for s in sessions:
                workspace_hint = f" [{s.workspace}]" if s.workspace else ""
                lines.append(f"  {s.id}: {s.name}{workspace_hint}")
            return "\n".join(lines), None

        # 切换 session
        original_session_id = session.session_id
        target_session = self.switch_session(original_session_id, args.strip())
        if target_session is None:
            return f"❌ Session 不存在: {args}", None

        new_session_id = target_session.session_id

        # emit NewSessionEvent + SyncSessionEvent
        event_bus.emit(
            NewSessionEvent(
                session_id=original_session_id,
                new_session_id=new_session_id,
                agent=_build_agent_info(target_session.agent),
                name=target_session.session_name,
                target=EventTarget(scope="session"),
            )
        )

        # SyncSessionEvent
        cm = target_session.agent.context_manager
        full_chain = cm.get_context_window()
        event_bus.emit(
            SyncSessionEvent(
                session_id=new_session_id,
                messages=_serialize_messages(full_chain),
                agent=_build_agent_info(target_session.agent),
                name=target_session.session_name,
                target=EventTarget(scope="session"),
            )
        )
        _emit_context_stats(target_session.agent)

        count, tokens = cm.get_context_stats()
        return (
            f"✅ 已切换到 session: {new_session_id}\n消息数: {count}, tokens: {tokens}",
            new_session_id,
        )

    async def _cmd_fork(
        self,
        session: Session,
        args: str,
        client_id: str | None,
    ) -> tuple[str, str | None]:
        """处理 /fork [uuid|list] 命令。"""
        cm = session.agent.context_manager

        if not args or args.strip() == "list":
            targets = cm.get_branch_targets()
            if not targets:
                return "❌ 没有可分叉的用户消息", None
            session.agent.emit(
                BranchTargetsEvent(
                    session_id=session.session_id,
                    targets=[BranchTargetInfo(**t) for t in targets],
                )
            )
            lines = ["📋 可分叉的用户消息:"]
            for item in targets:
                lines.append(f"  {item['uuid']}  {item['content']}")
            lines.append("\n使用 /fork <uuid> 从指定消息分叉")
            return "\n".join(lines), None

        target_uuid = args.strip()
        original_session_id = session.session_id

        result = self.fork_session(original_session_id, target_uuid)
        if result is None:
            return f"❌ Session 不存在: {original_session_id}", None

        new_session, draft = result
        new_session_id = new_session.session_id

        event_bus.emit(
            NewSessionEvent(
                session_id=original_session_id,
                new_session_id=new_session_id,
                agent=_build_agent_info(new_session.agent),
                name=new_session.session_name,
                target=EventTarget(scope="session"),
            )
        )
        event_bus.emit(
            SyncSessionEvent(
                session_id=new_session_id,
                messages=_serialize_messages(
                    list(new_session.agent.context_manager._messages)
                ),
                agent=_build_agent_info(new_session.agent),
                name=new_session.session_name,
                draft=draft,
                target=EventTarget(scope="session"),
            )
        )
        _emit_context_stats(new_session.agent)
        return (
            f"✅ 已从 uuid={target_uuid} 分叉到新 session: {new_session_id}",
            new_session_id,
        )

    async def _cmd_title(
        self,
        session: Session,
        args: str,
    ) -> tuple[str, None]:
        """处理 /title [title] 命令。"""
        if not args:
            current_name = session.session_name or "(未命名)"
            return f"当前 session 名称: {current_name}", None
        title = args.strip()
        session.set_title(title)
        return f"✅ Session 名称已设置为: {title}", None

    def _cmd_info(self, session: Session) -> tuple[str, None]:
        """处理 /info 命令。"""
        status = session.agent.get_status()
        event_bus.emit(
            SystemInfoEvent(
                session_id=session.session_id,
                model=status["model"],
                api_url=status.get("api_url", "unknown"),
                tools=status.get("tools", []),
                total_tokens=status.get("total_tokens", 0),
                context_window_tokens=status.get("context_window_tokens", 0),
                thinking=status.get("thinking", False),
                session_name=session.session_name,
                target=EventTarget(scope="session"),
            )
        )
        return (
            f"model: {status['model']}\n"
            f"api: {status.get('api_url', 'unknown')}\n"
            f"tools: {', '.join(status.get('tools', [])) or 'none'}",
            None,
        )

    async def _cmd_agents(
        self,
        session: Session,
        args: str,
    ) -> tuple[str, None]:
        """处理 /agents [name] 命令。

        无参数时列出所有模板，有参数时切换到指定模板。
        """
        tm = self._template_manager

        if not args:
            # 列出所有模板
            agent_names = tm.all_names
            event_bus.emit(
                AgentListEvent(
                    session_id=session.session_id,
                    agents=agent_names,
                    current_agent=session.template_name,
                    target=EventTarget(scope="session"),
                )
            )
            lines = ["📋 Agent 模板:"]
            for name in agent_names:
                marker = " ← current" if name == session.template_name else ""
                default_marker = " (default)" if name == tm.default_name else ""
                lines.append(f"  - {name}{default_marker}{marker}")
            return "\n".join(lines), None

        # 切换到指定模板
        template_name = args.strip()
        template = tm.get(template_name)
        if template is None:
            return (
                f"❌ Agent 模板 '{template_name}' 不存在。\n"
                f"可用模板: {', '.join(tm.all_names)}",
                None,
            )
        old_model = session.agent.model
        await session.switch_template(template)
        event_bus.emit(
            ModelSwitchedEvent(
                session_id=session.session_id,
                old_model=old_model,
                new_model=template.model,
            )
        )
        return f"✅ 已切换到 agent 模板: {template_name}", None

    async def _dispatch_magic_command(
        self, session: Session, content: str
    ) -> str | None:
        """解析并执行魔术命令。

        匹配不到注册命令时返回 None——SM 将原内容交给 agent 处理。
        """
        parts = content.lstrip("/").split(maxsplit=1)
        cmd_name = parts[0]
        args = parts[1] if len(parts) > 1 else ""

        cmd = magic_registry.get(cmd_name)
        if cmd is None:
            return None

        handler = cmd.handler
        result = await handler(session.agent, args)
        return result


# ============================================================
# 辅助函数 — SM 内部使用
# ============================================================


def _build_agent_info(agent: Any) -> AgentInfo:
    """从 WingAgent 构造 AgentInfo。"""
    cm = agent.context_manager
    return AgentInfo(
        model_name=agent.model,
        system_prompt=cm.system_prompt.content if cm.system_prompt else None,
        tools=[t.name for t in agent.tools],
        skills=list(cm._skills_cache.keys()),
        rules=list(cm._rules_patterns),
    )


def _serialize_messages(messages: list[Message]) -> list[dict]:
    """将消息序列化为 dict 列表，用于 SyncSessionEvent。"""
    result = []
    for msg in messages:
        d: dict = {
            "role": msg.role,
            "content": msg.content or "",
            "uuid": msg.uuid,
        }
        if msg.reasoning_content:
            d["reasoning_content"] = msg.reasoning_content
        if msg.tool_calls:
            d["tool_calls"] = [
                {"id": tc.id, "name": tc.name, "arguments": tc.arguments}
                for tc in msg.tool_calls
            ]
        if msg.tool_call_id:
            d["tool_call_id"] = msg.tool_call_id
        result.append(d)
    return result


def _emit_context_stats(agent: Any) -> None:
    """emit ContextStatsEvent（scope=session，仅发给订阅了该 session 的 client）。"""
    cm = agent.context_manager
    count, tokens = cm.get_context_stats()
    ctx_window = 0
    if cm.compactor:
        ctx_window = cm.compactor.context_window_tokens
    event_bus.emit(
        ContextStatsEvent(
            session_id=agent.session_id,
            message_count=count,
            total_tokens=tokens,
            context_window_tokens=ctx_window,
            target=EventTarget(scope="session"),
        )
    )


def _list_sessions(
    sessions_path: Path, workspace: str | None = None
) -> list[SessionInfo]:
    """列出所有有效 session，支持 workspace 优先排序。

    排序规则（stable sort）：
    1. 第一优先级：相同 workspace 的 session 排在前面
    2. 第二优先级：last_interaction 降序（最近在前）
    """
    if not sessions_path.exists():
        return []

    result = []
    for session_dir in sessions_path.iterdir():
        if not session_dir.is_dir():
            continue
        newest_file = session_dir / "newest.json"
        if not newest_file.exists():
            continue

        metadata_file = session_dir / "metadata.json"
        session_name = None
        session_workspace = None
        last_interaction = None

        if metadata_file.exists():
            try:
                data = json.loads(metadata_file.read_text())
                session_name = data.get("session_name")
                session_workspace = data.get("workspace")
                last_interaction = data.get("last_interaction")
            except Exception:
                pass

        if session_name is None:
            try:
                data = json.loads(newest_file.read_text())
                for msg in data:
                    if msg.get("role") == "user":
                        session_name = msg.get("content", "")[:100]
                        break
            except Exception:
                continue

        if session_name:
            result.append(
                SessionInfo(
                    id=session_dir.name,
                    name=session_name,
                    workspace=session_workspace,
                    last_interaction=last_interaction,
                )
            )

    # 归一化排序键
    def _timestamp_key(s: SessionInfo) -> float:
        ts = s.last_interaction
        if ts is not None:
            if isinstance(ts, str):
                try:
                    dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
                    return dt.timestamp()
                except Exception:
                    pass
            elif isinstance(ts, (int, float)):
                return float(ts)
        prefix = s.id[:15] if len(s.id) >= 15 else s.id
        try:
            dt = datetime.strptime(prefix, "%Y%m%d-%H%M%S")
            return dt.timestamp()
        except Exception:
            return 0.0

    result.sort(key=_timestamp_key, reverse=True)

    if workspace:
        norm_ws = os.path.normpath(workspace)
        result.sort(key=lambda s: os.path.normpath(s.workspace or "") != norm_ws)

    return result
