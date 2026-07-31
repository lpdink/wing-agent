# wing/session_manager.py
"""
wing/session_manager.py — SessionManager

管理 session 生命周期、消息路由、魔术命令分发。

核心设计约束：
  - SessionManager 只处理跨 session 行为（create、switch、fork、列表、id 模糊匹配）
  - 单 session 内部逻辑（metadata 管理、title 设置）在 Session 中
  - 持久状态统一经由 SessionStore——SM 不直接与存储介质打交道
  - WingAgent 不知道 SessionManager 的存在——它通过 EventBus 投递事件。
  - EventBus 不知道 WingAgent 的存在——它只是路由管道。
  - 路由表由 WingRuntime 统一管理，SM 不感知 client_id 和 contextvars。
  - _post() 采用两级分发：/ 开头走 magic_registry，否则走 session.post()
"""

from __future__ import annotations

import uuid
from datetime import datetime
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
from wing.store import SessionMetadata, SessionStore

if TYPE_CHECKING:
    from wing.gateway.protocol import AgentOverride


def _remap_chain_uuids(messages: list[Message]) -> list[Message]:
    """深拷贝消息链并重映射 uuid/parent_uuid/unzip_last_uuid，保持拓扑。

    深拷贝确保 fork 不污染源 session 的内存链状态
    （extract_subchain 返回的是源 TrackedList 中的 live 对象）。
    """
    copies = [msg.model_copy(deep=True) for msg in messages]

    uuid_map: dict[str, str] = {}
    for msg in copies:
        if msg.uuid:
            uuid_map[msg.uuid] = str(uuid.uuid4())

    for msg in copies:
        if msg.uuid:
            msg.uuid = uuid_map.get(msg.uuid, str(uuid.uuid4()))
        if msg.parent_uuid:
            msg.parent_uuid = uuid_map.get(msg.parent_uuid)
        if msg.unzip_last_uuid:
            msg.unzip_last_uuid = uuid_map.get(msg.unzip_last_uuid)

    return copies


class SessionManager:
    """管理 session 生命周期、消息路由、魔术命令分发。

    每个 Session 有独立的 WingAgent。
    事件通过全局 EventBus 投递。

    持久化经由 store 注册表（如 {"file": FileSessionStore, "memory": MemorySessionStore}），
    创建 session 时选择后端，Session 持有自己所属的 store 引用。

    内部方法（_ 开头）：
      - _post：消息路由入口，由 WingRuntime 调用
      - _dispatch_magic_command：路由到 magic_registry

    外部方法：
      - create_session：创建新 session（可选 backend）
      - fork_session：从指定消息分叉（继承源 session 后端）
      - resume_session：恢复已有 session（精确匹配 session id）

    路由表由 WingRuntime 统一管理，SM 不感知 client_id 和 contextvars。
    """

    def __init__(
        self,
        stores: dict[str, SessionStore],
        default_backend: str = "file",
    ) -> None:
        if not stores:
            raise ValueError("stores cannot be empty")
        if default_backend not in stores:
            raise ValueError(
                f"default backend '{default_backend}' not in stores: {list(stores)}"
            )
        self._sessions: dict[str, Session] = {}
        self._stores = stores
        self._default_backend = default_backend

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
        backend: str | None = None,
    ) -> Session:
        """创建新 session。

        Args:
            template_name: Agent 模板名称，None 时使用默认模板
            session_id: 指定 session_id（磁盘恢复场景），None 时自动生成
            workspace: 工作目录
            agent_override: AgentOverride 参数覆盖（None 字段不覆盖 template 值）
            backend: 存储后端名称（如 file/memory），None 时使用默认后端

        Raises:
            ValueError: 模板不存在或 backend 未知
        """
        backend_name = backend if backend is not None else self._default_backend
        store = self._stores.get(backend_name)
        if store is None:
            raise ValueError(
                f"Unknown storage backend '{backend_name}'. "
                f"Available: {list(self._stores)}"
            )

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

        # 创建 TrackedList（经 store 打开消息日志）
        messages: TrackedList[Message] = (
            TrackedList.load(store.open_log(sid), Message)
            if session_id is not None
            else TrackedList(store.open_log(sid))
        )

        session = Session.from_template(
            template=template,
            session_id=sid,
            messages=messages,
            store=store,
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

    # ── resolve ───────────────────────────────

    def _resolve_with_store(self, session_id: str) -> tuple[str, SessionStore] | None:
        """跨 stores 精确解析 session id，返回 (session_id, store)。

        优先命中内存中的 session，再按 stores 注册顺序查后端是否存在。
        """
        if session_id in self._sessions:
            return session_id, self._sessions[session_id].store
        for store in self._stores.values():
            if store.exists(session_id):
                return session_id, store
        return None

    def resume_session(
        self,
        session_id: str,
        template: "AgentTemplate | None" = None,
    ) -> Session:
        """恢复已有 session（精确匹配 session id）。已在内存中则直接返回。

        模板解析优先级：显式传入 > metadata.template_name > 默认模板。

        Args:
            session_id: 目标 session ID（须为完整 ID）
            template: 可选模板。传入时使用该模板恢复；
                      不传时优先使用 metadata 中持久化的模板。

        Returns:
            恢复后的 Session 实例

        Raises:
            LookupError: session 不存在
        """
        result = self._resolve_with_store(session_id)
        if result is None:
            raise LookupError(f"Session not found: {session_id}")
        resolved, store = result

        existing = self._sessions.get(resolved)
        if existing is not None:
            return existing

        metadata = store.load_metadata(resolved)

        # 模板解析：显式传入 > metadata.template_name > 默认
        tpl = template
        if tpl is None and metadata is not None and metadata.template_name is not None:
            tpl = self._template_manager.get(metadata.template_name)
        if tpl is None:
            tpl = self._template_manager.default

        messages: TrackedList[Message] = TrackedList.load(
            store.open_log(resolved), Message
        )
        # workspace 从 metadata 恢复，使 CM 构造时即以正确工作目录加载相对路径的
        # rules/skills。漏传会让 CM 以 workspace=None 构建、patterns 退化到进程
        # cwd 展开，把错误文件加载进系统提示词（Session.__init__ 的事后回填救不
        # 回来——rules/skills 在 CM.__init__ 里就已急切加载并缓存）。
        session = Session.from_template(
            template=tpl,
            session_id=resolved,
            messages=messages,
            store=store,
            workspace=metadata.workspace if metadata is not None else None,
        )
        self._sessions[resolved] = session
        log.info(f"Session resumed: {resolved}")
        return session

    def fork_session(
        self,
        session_id: str,
        target_uuid: str,
    ) -> tuple[Session, str | None] | None:
        """从指定 session 的 target_uuid 处 fork 出新 session。

        新 session 继承源 session 的后端。消息与元数据均经由源 session
        所属 store 写入：元数据（workspace/forked_from/template_name/
        last_interaction）一次写全——fork 的正确性由 store 单一所有者保证。
        """
        source = self._sessions.get(session_id)
        if source is None:
            return None

        cm = source.context_manager
        try:
            subchain, draft = cm.extract_subchain(target_uuid)
        except ValueError:
            log.warning(f"fork_session: uuid {target_uuid} not found")
            return None

        store = source.store
        new_session_id = self._generate_session_id()

        # 深拷贝 + uuid 重映射（不污染源 session）
        remapped = _remap_chain_uuids(subchain)

        # 写入消息（经 store 打开日志）
        new_messages: TrackedList[Message] = TrackedList(store.open_log(new_session_id))
        if remapped:
            new_messages.extend_detached(remapped)

        # 一次写全元数据——fork bug 的结构性修复
        store.save_metadata(
            new_session_id,
            SessionMetadata(
                workspace=source.session_workspace,
                forked_from=session_id,
                template_name=source.template_name,
                last_interaction=datetime.now().isoformat(),
            ),
        )

        # 从旧 agent 抽取模板，构建新 session
        template = AgentTemplate.from_agent(source.agent, name=source.template_name)
        new_session = Session.from_template(
            template=template,
            session_id=new_session_id,
            messages=new_messages,
            store=store,
            workspace=source.session_workspace,
        )
        self._sessions[new_session_id] = new_session

        return new_session, draft

    # ============================================================
    # 外部方法：查询
    # ============================================================

    def list_sessions(self) -> list[SessionInfo]:
        """列出所有有效 session（跨 stores 聚合），按 last_interaction 时间降序。

        每个 session 携带运行时 `status`：
        - 已加载进内存（在 `self._sessions` 中）→ 取 live 状态（idle/working/waiting）
        - 未 resume → `inactive`

        workdir 优先排序属于前端业务语义，不在此处处理。
        """
        result = []
        for store in self._stores.values():
            for summary in store.list_summaries():
                metadata = summary.metadata
                name = metadata.session_name or summary.first_user_message
                if not name:
                    continue

                loaded = self._sessions.get(summary.id)
                result.append(
                    SessionInfo(
                        id=summary.id,
                        name=name,
                        workspace=metadata.workspace,
                        last_interaction=metadata.last_interaction,
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
