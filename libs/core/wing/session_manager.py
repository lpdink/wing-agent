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
  - _post() 采用两级分发：/ 开头走 magic_registry，否则走 session.post()
"""

from __future__ import annotations

import json
import os
import uuid
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

from wing.agent_template import AgentTemplate, AgentTemplateManager
from wing.common.logger import log
from wing.common.tracked_list import TrackedList
from wing.common.utils import generate_session_id
from wing.config import get_config
from wing.hook_registry import hooks
from wing.event import (
    DeliveredEvent,
    EventTarget,
    SessionInfo,
)
from wing.event_bus import event_bus
from wing.magic_command.prompt_commands import expand_prompt_command
from wing.schema import Message
from wing.session import Session

if TYPE_CHECKING:
    from wing.gateway.protocol import AgentOverride


class SessionManager:
    """管理 session 生命周期、消息路由、魔术命令分发。

    每个 Session 有独立的 WingAgent。
    事件通过全局 EventBus 投递。

    内部方法（_ 开头）：
      - _post：消息路由入口，由 WingRuntime 调用
      - _dispatch_magic_command：路由到 magic_registry

    外部方法：
      - create_session：创建新 session
      - fork_session：从指定消息分叉
      - resume_session：恢复已有 session（支持模糊匹配）

    路由表由 WingRuntime 统一管理，SM 不感知 client_id 和 contextvars。
    """

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

    def iter_sessions(self) -> list[Session]:
        """返回所有活跃 session 的列表（快照）。"""
        return list(self._sessions.values())

    def _generate_session_id(self) -> str:
        """生成唯一 session id（委托 common.utils.generate_session_id）。"""
        return generate_session_id()

    def create_session(
        self,
        template_name: str | None = None,
        session_id: str | None = None,
        workspace: str | None = None,
        agent_override: AgentOverride | None = None,
    ) -> Session:
        """创建新 session。

        Args:
            template_name: Agent 模板名称，None 时使用默认模板
            session_id: 指定 session_id（磁盘恢复场景），None 时自动生成
            workspace: 工作目录
            agent_override: AgentOverride 参数覆盖（None 字段不覆盖 template 值）
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

        # 应用 agent override（在 session 完全构建后覆盖特定字段）
        if agent_override is not None:
            session.apply_agent_override(agent_override)

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
        """解析 session id（支持模糊匹配），返回完全匹配的 session_id。

        优先精确匹配内存中的 session，再走文件系统模糊匹配。
        """
        if session_id in self._sessions:
            return session_id
        matched_path = self._resolve_session(session_id)
        if matched_path is None:
            return None
        return matched_path.name

    def resume_session(
        self,
        session_id: str,
        template: "AgentTemplate | None" = None,
    ) -> Session:
        """恢复已有 session（支持模糊匹配）。已在内存中则直接返回。

        Args:
            session_id: 目标 session ID（支持前缀/包含匹配）
            template: 可选模板。传入时使用该模板恢复（如从 source agent 反推）；
                      不传时使用默认模板。

        Returns:
            恢复后的 Session 实例

        Raises:
            LookupError: session 不存在
        """
        resolved = self.resolve_session_id(session_id)
        if resolved is None:
            raise LookupError(f"Session not found: {session_id}")

        existing = self._sessions.get(resolved)
        if existing is not None:
            return existing

        tpl = template or self._template_manager.default
        messages: TrackedList[Message] = TrackedList.load(
            self._sessions_path / resolved, Message
        )
        session = Session.from_template(
            template=tpl,
            session_id=resolved,
            messages=messages,
        )
        self._sessions[resolved] = session
        log.info(f"Session resumed: {resolved}")
        return session

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

    # ============================================================
    # 外部方法：查询
    # ============================================================

    def list_sessions(self) -> list[SessionInfo]:
        """列出所有有效 session，按 last_interaction 时间降序（最近在前）。

        每个 session 携带运行时 `status`：
        - 已加载进内存（在 `self._sessions` 中）→ 取 live 状态（idle/working/waiting）
        - 仅在磁盘、未 resume → `inactive`

        workdir 优先排序属于前端业务语义，不在此处处理。
        """
        if not self._sessions_path.exists():
            return []

        result = []
        for session_dir in self._sessions_path.iterdir():
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
                loaded = self._sessions.get(session_dir.name)
                result.append(
                    SessionInfo(
                        id=session_dir.name,
                        name=session_name,
                        workspace=session_workspace,
                        last_interaction=last_interaction,
                        status=loaded.status if loaded is not None else "inactive",
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

        return result

    # ============================================================
    # 外部方法：消息路由入口
    # ============================================================

    async def _post(
        self,
        content: str,
        request_id: str | None = None,
        session_id: str | None = None,
        client_id: str | None = None,
        tool_call_id: str | None = None,
    ) -> None:
        """路由消息到指定 session。（内部方法，由 WingRuntime 调用）

        Contextvars 由 WingRuntime.post() 统一管理，此方法不设置/恢复。

        - / 开头且匹配 prompt 命令 → 展开为纯文本后投递
        - 否则 → 直接投递给 session.post()
        """
        assert session_id is not None, "session_id is required by WingRuntime"
        session = self._sessions.get(session_id)
        if session is None:
            log.error(f"Session not found: {session_id}")
            return

        # DeliveredEvent（总是 emit）
        event_bus.emit(
            DeliveredEvent(
                session_id=session.session_id,
                target=EventTarget(scope="session"),
            )
        )

        # Prompt 命令展开：/ 开头 → 尝试展开 → 展开成功则用展开文本投递
        if content.startswith("/"):
            parts = content.lstrip("/").split(maxsplit=1)
            cmd_name = parts[0]
            args = parts[1] if len(parts) > 1 else ""
            expanded = expand_prompt_command(cmd_name, args)
            if expanded is not None:
                content = expanded

        log.info(
            f"SM._post: routing '{content[:50]}' to session.post (session={session_id})"
        )
        await session.post(content, request_id=request_id, tool_call_id=tool_call_id)
