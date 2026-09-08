# wing/session.py
"""
wing/session.py — Session 类

Session 是有行为的对象，在构造器中创建 ContextManager 和 WingAgent。
负责单 session 内部操作：metadata 管理、第一条消息自动 title。

设计约束：
  - Session 通过 from_template 类方法创建（统一入口）
  - Session 不反向引用 agent（agent 在 Session 之下）
  - Session 不直接与存储介质打交道——持久状态全部经由 SessionStore
    （metadata 读写、消息日志均委托 store；TrackedList 在第一条消息
    到来时经 MessageLog 创建底层存储）
"""

from __future__ import annotations

from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.common.tracked_list import TrackedList
from wing.config import get_config
from wing.context_manager import ContextManager
from wing.provider import create_provider
from wing.schema import ChainNode, Message
from wing.store import SessionMetadata, SessionStore

if TYPE_CHECKING:
    from wing.agent import WingAgent
    from wing.agent_template import AgentTemplate
    from wing.event.base import AgentInfo, SessionStatus
    from wing.gateway.protocol import AgentOverride


def serialize_message(msg: Message) -> dict:
    """Message → 前端重放投影 dict。

    SyncSessionEvent.messages（已提交）与 uncommitted（未提交 assistant
    投影）共用此形状——前端经同一条 replay_messages 路径渲染。assistant 的
    content / reasoning_content / tool_calls 实时派生自 content_blocks。
    """
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
    return d


class Session:
    """有行为的 Session 对象。

    通过 from_template 类方法创建，在构造器中创建 ContextManager 和 WingAgent。
    负责 session 内部操作：metadata 管理、第一条消息自动 title。
    """

    def __init__(
        self,
        session_id: str,
        messages: TrackedList[ChainNode],
        context_manager: ContextManager,
        agent: "WingAgent",
        store: SessionStore,
        workspace: str | None = None,
    ) -> None:
        """底层构造器——由 from_template 调用，不建议直接使用。"""
        self._session_id = session_id
        self._messages = messages
        self._store = store
        self._template_name: str | None = None

        # 从 store 加载已有 metadata（磁盘恢复场景），与 workspace 参数合并
        metadata = store.load_metadata(session_id) or SessionMetadata()
        if workspace is not None:
            metadata.workspace = workspace
        self._metadata = metadata

        self._context_manager = context_manager
        self._agent = agent

        # 将 workspace 注入 agent 作为 Bash 工具的 cwd。
        # 无论是新建（workspace 参数）还是磁盘恢复（metadata），
        # 都在此处统一设置，确保 resume 后 agent cwd 正确。
        if self._metadata.workspace:
            self._agent.set_cwd(Path(self._metadata.workspace).resolve())

        self._initial_status = self._agent.get_status()

        log.info(f"Session initialized: {session_id}")

    @classmethod
    def from_template(
        cls,
        template: "AgentTemplate",
        session_id: str,
        messages: TrackedList[ChainNode],
        store: SessionStore,
        workspace: str | None = None,
    ) -> "Session":
        """从模板构建全新 Session。

        Args:
            template: 解析后的 AgentTemplate
            session_id: Session ID
            messages: TrackedList 消息列表
            store: 该 session 所属的 SessionStore
            workspace: 工作目录
        """
        from wing.agent import WingAgent

        # 创建 ContextManager
        context_manager = ContextManager(
            session_id=session_id,
            messages=messages,
            system_prompt=template.system_prompt,
            compactor=template.compactor,
            skills_patterns=template.skills_patterns,
            rules_patterns=template.rules_patterns,
            workspace=workspace,
        )

        # 创建 WingAgent
        provider_cfg = get_config().get_provider(template.provider_name)
        agent = WingAgent(
            model=template.model,
            model_provider=create_provider(provider_cfg, session_id=session_id),
            stream=True,
            context_manager=context_manager,
            tools=template.resolved_tools,
            max_turns=template.max_turns,
            yolo=template.yolo,
        )

        session = cls(
            session_id=session_id,
            messages=messages,
            context_manager=context_manager,
            agent=agent,
            store=store,
            workspace=workspace,
        )
        session._template_name = template.name
        session._metadata.template_name = template.name
        return session

    async def switch_template(self, template: "AgentTemplate") -> None:
        """原地替换 agent，保留消息历史。

        在同一个 session 内：
        1. await 旧 agent.shutdown() 清空 inbox 并 cancel worker
        2. 用同一个 TrackedList 构建新 ContextManager
        3. 用新模板创建新 WingAgent

        不产生 session 切换事件，SM 和 TUI 不感知 session 变化。
        """
        from wing.agent import WingAgent

        # 1. 干净关闭旧 agent（清空 inbox + cancel worker + await 完成），
        #    并关闭其拥有的全部 provider client（新 agent 从空表开始——
        #    旧缓存连同已关闭的 client 一起清除，不会被后续切换交回）。
        old_agent = self._agent
        await old_agent.shutdown()
        await old_agent.aclose_providers()

        # 2. 用同一个 TrackedList 构建新 ContextManager
        self._context_manager = ContextManager(
            session_id=self._session_id,
            messages=self._messages,
            system_prompt=template.system_prompt,
            compactor=template.compactor,
            skills_patterns=template.skills_patterns,
            rules_patterns=template.rules_patterns,
            workspace=self._metadata.workspace,
        )

        # 3. 创建新 Agent
        provider_cfg = get_config().get_provider(template.provider_name)
        self._agent = WingAgent(
            model=template.model,
            model_provider=create_provider(provider_cfg, session_id=self._session_id),
            stream=True,
            context_manager=self._context_manager,
            tools=template.resolved_tools,
            max_turns=template.max_turns,
            yolo=template.yolo,
        )

        cwd = (
            str(Path(self._metadata.workspace).resolve())
            if self._metadata.workspace
            else None
        )
        self._agent.set_cwd(Path(cwd) if cwd else None)

        self._template_name = template.name
        self._metadata.template_name = template.name
        self._save_metadata()
        self._initial_status = self._agent.get_status()
        log.info(f"Session {self._session_id}: switched to agent '{template.name}'")

    def apply_agent_override(self, override: AgentOverride) -> None:
        """应用 AgentOverride 到当前 session 的 agent。

        在 from_template 之后调用，覆盖 template 中的特定字段。
        通过 agent 自身的公共方法完成覆盖，不直接操作内部状态。

        Override 语义：
        - None 字段不覆盖（保留 template 值）
        - system_prompt 替换，append_system_prompt 追加
        - 两者同时存在时，先替换再追加
        """
        cm = self._context_manager
        agent = self._agent

        # 1. model 覆盖（若指定 provider 则切换 provider，否则用当前）
        if override.model is not None:
            if override.provider is not None:
                provider = agent.get_or_create_provider(override.provider)
            else:
                provider = agent.model_provider
            agent.set_model(override.model, provider)

        # 2. system_prompt 替换（先替换，后追加，保证顺序正确）
        if override.system_prompt is not None:
            cm.setin_system_prompt = override.system_prompt

        # 3. append_system_prompt 追加
        if override.append_system_prompt is not None:
            current = cm.setin_system_prompt or ""
            cm.setin_system_prompt = current + "\n" + override.append_system_prompt

        # 4. tools 覆盖（从 registry 获取新的未绑定工具，避免闭包泄漏）
        if override.tools is not None:
            agent.set_tools(override.tools)

        # 5. max_turns 覆盖
        if override.max_turns is not None:
            agent.set_max_turns(override.max_turns)

        # 6. effort (reasoning_effort) 覆盖
        if override.effort is not None:
            agent.set_reasoning_effort(override.effort)

        # 7. yolo 覆盖
        if override.yolo is not None:
            agent.set_yolo(override.yolo)

        log.info(
            f"Session {self._session_id}: applied agent override "
            f"(model={override.model}, provider={override.provider}, "
            f"tools={override.tools}, "
            f"max_turns={override.max_turns}, effort={override.effort}, "
            f"yolo={override.yolo})"
        )

    # ── 暴露属性 ──────────────────────────────────

    @property
    def agent(self) -> "WingAgent":
        return self._agent

    @property
    def template_name(self) -> str | None:
        """当前 session 使用的 agent 模板名称。"""
        return self._template_name

    @property
    def session_id(self) -> str:
        return self._session_id

    @property
    def context_manager(self) -> ContextManager:
        return self._context_manager

    @property
    def initial_status(self) -> dict:
        return self._initial_status

    @property
    def session_name(self) -> str | None:
        """当前 session 名称（来自 metadata）。"""
        return self._metadata.session_name

    @property
    def session_workspace(self) -> str | None:
        """当前 session 的工作目录（来自 metadata）。"""
        return self._metadata.workspace

    @property
    def status(self) -> "SessionStatus":
        """Session 运行时状态（idle/working/waiting），委托 agent 推导。"""
        return self._agent.status

    def set_workspace(self, path: str) -> None:
        """切换 session 工作目录。

        校验路径合法性（存在且为目录），更新 agent cwd、
        ContextManager workspace，并持久化到 metadata.json。

        Args:
            path: 目标路径（支持 ~ 展开）

        Raises:
            ValueError: 路径不存在或不是目录
        """
        resolved = Path(path).expanduser().resolve()
        if not resolved.exists():
            raise ValueError(f"workspace path does not exist: {resolved}")
        if not resolved.is_dir():
            raise ValueError(f"workspace path is not a directory: {resolved}")

        resolved_str = str(resolved)
        self._metadata.workspace = resolved_str
        self._agent.set_cwd(resolved)
        self._context_manager._workspace = resolved
        self._save_metadata()
        log.info(f"Session {self._session_id}: workspace changed to {resolved_str}")

    # ── 序列化方法 ──────────────────────────────────

    def to_agent_info(self) -> "AgentInfo":
        """从 Session 的 agent 和 context_manager 构造 AgentInfo。"""
        from wing.event.base import AgentInfo

        cm = self._context_manager
        return AgentInfo(
            model_name=self._agent.model,
            system_prompt=cm.system_prompt.content if cm.system_prompt else None,
            tools=[t.effective_llm_name for t in self._agent.tools],
            skills=list(cm._skills_cache.keys()),
            rules=list(cm._rules_files),
            workspace=self._metadata.workspace,
        )

    def serialize_messages(self) -> list[dict]:
        """序列化 context window 中的消息为 dict 列表。"""
        cm = self._context_manager
        return [serialize_message(msg) for msg in cm.get_context_window()]

    @property
    def last_interaction(self) -> str | None:
        """最后一次用户互动时间（ISO 8601 格式）。"""
        return self._metadata.last_interaction

    @property
    def store(self) -> SessionStore:
        """该 session 所属的 SessionStore（fork 继承后端、SM 聚合用）。"""
        return self._store

    # ── metadata 管理 ──────────────────────────────

    def _save_metadata(self) -> None:
        """将当前 metadata 模型保存到 store。"""
        self._store.save_metadata(self._session_id, self._metadata)

    def set_title(self, title: str) -> None:
        """设置 session 标题并持久化。"""
        self._metadata.session_name = title
        self._save_metadata()

    async def update_state(
        self,
        *,
        model: str | None = None,
        provider_name: str | None = None,
        template: "AgentTemplate | None" = None,
        title: str | None = None,
        thinking: bool | None = None,
        reasoning_effort: str | None = None,
        yolo: bool | None = None,
        workspace: str | None = None,
        tools: list[str] | None = None,
    ) -> None:
        """更新 session 状态。按 template → model → tools → title → thinking → effort → yolo → workspace 顺序执行。

        Args:
            model: 切换模型（裸模型名）
            provider_name: 切换 provider（配合 model 使用）
            template: 切换 agent 模板（None 表示不切换）
            title: 设置标题
            thinking: 开关 thinking 模式
            reasoning_effort: 推理力度
            yolo: 开关 yolo 模式
            workspace: 切换工作目录
            tools: 切换工具集（全量替换，ref 格式）
        """
        # 纯校验：任何字段非法在 mutation 之前退出，避免部分应用
        if tools is not None:
            from wing.tool_registry import tool_registry

            for ref in tools:
                if tool_registry.resolve(ref) is None:
                    raise ValueError(f"cannot resolve tool reference: '{ref}'")

        if template is not None:
            await self.switch_template(template)

        if model is not None:
            self._apply_model(model, provider_name)

        if tools is not None:
            self.agent.set_tools(tools)

        if title is not None:
            self.set_title(title)

        if thinking is not None:
            self.agent.model_provider.set_thinking(thinking)

        if reasoning_effort is not None:
            self.agent.set_reasoning_effort(reasoning_effort)

        if yolo is not None:
            self.agent.set_yolo(yolo)

        if workspace is not None:
            self.set_workspace(workspace)

    def _apply_model(self, model: str, provider_name: str | None = None) -> None:
        """切换模型，必要时切换 provider——委托 agent 的单一持有能力。

        Session 不持有 provider：provider client 表归 WingAgent（按 name 有界
        持有、切回同名复用、跨 provider 切模型不关闭旧 client）。
        """
        if provider_name is None or provider_name == self.agent.model_provider.name:
            provider = self.agent.model_provider
        else:
            provider = self.agent.get_or_create_provider(provider_name)
        self.agent.set_model(model, provider)

    def touch_last_interaction(self) -> None:
        """更新最后互动时间并持久化。"""
        self._metadata.last_interaction = datetime.now().isoformat()
        self._save_metadata()

    # ── 第一条消息 metadata 写入 ──────────────────

    def _check_first_message_metadata(self, content: str) -> None:
        """首条用户消息时设置标题（仅变更模型，不落盘）。

        判断条件是 metadata.session_name 为 None（而非存储中是否存在记录）——
        fork 产生的 session 已有 metadata（forked_from 等），但标题仍为空，
        应正常获得自动标题，且保存时不会覆盖其他字段（模型整体保存）。

        持久化由 post 流程中紧随其后的 touch_last_interaction 一次性完成，
        避免首条消息两次连续落盘。
        """
        if self._metadata.session_name is not None:
            return  # 已有标题，不是第一条消息
        self._metadata.session_name = content[:100]

    async def post(
        self,
        content: str,
        request_id: str | None = None,
        tool_call_id: str | None = None,
    ) -> None:
        """投递用户消息。

        先检查并写入第一条消息 metadata，更新最后互动时间，再转发给 agent。
        标题与 last_interaction 合并为一次 metadata 落盘（touch_last_interaction）。
        tool_call_id 非空时表示这是对某个 Ask 事件的定向回复。
        """
        self._check_first_message_metadata(content)
        self.touch_last_interaction()
        await self._agent.post(
            content, request_id=request_id, tool_call_id=tool_call_id
        )
