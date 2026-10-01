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
from wing.schema import ChainNode, Message, Tool
from wing.store import SessionMetadata, SessionStore
from wing.tool_registry import ToolRef

if TYPE_CHECKING:
    from wing.agent import WingAgent
    from wing.agent_template import AgentTemplate
    from wing.event.base import AgentInfo, SessionStatus
    from wing.gateway.protocol import AgentOverride
    from wing.provider import ModelProvider


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


def tool_refs(tools: list[Tool]) -> list[str]:
    """把 Tool 列表投影为可持久化的 ref 列表（"Bash" / "client.Read"）。

    与 `WingAgent.set_tools()` 接受的引用格式一致——ref 可在任何进程经
    `tool_registry.resolve()` 还原；闭包 / dispatch 闭包不可持久化，这正是
    "只存引用不存对象"的原因。会话状态持久化（metadata.tools）与 fork 的
    工具集对齐共用本投影。
    """
    return [str(ToolRef(namespace=t.namespace, name=t.name)) for t in tools]


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

        # 持久状态还原：模型绑定 / 系统提示词 / 工具集 / 动态开关——构造时
        # 统一应用，覆盖 resume（重启 / 逐出后水合）与 fork 子会话构造两条
        # 路径（session id 已是既有的、记录已在磁盘上）。记录存在时优先于
        # 模板默认。
        self._restore_persisted_state()

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
        # 追加系统提示词是会话级内容（hook 注入的环境信息 + 追加指令），
        # 与模板无关：新 CM 上重新挂载持久化值（否则重启/切换后丢失）。
        if self._metadata.append_system_prompt:
            self._context_manager.append_system_prompt = (
                self._metadata.append_system_prompt
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
        # 模板切换以新模板为准——此前记录的显式覆盖（基础提示词 / 工具 /
        # 动态开关 / 限额）随切换作废：清记录即「跟随新模板与配置」的持久化
        # 表达（新 agent 已按新模板构造），避免重启后把旧覆盖又贴回新 agent。
        self._metadata.system_prompt = None
        self._metadata.tools = None
        self._metadata.thinking = None
        self._metadata.reasoning_effort = None
        self._metadata.yolo = None
        self._metadata.max_turns = None
        # 模板切换覆写模型记录（新模板的生效模型）——与 template_name 及
        # 上述清理同一次落盘（_persist_model 保存整个 metadata）。
        self._persist_model()
        self._initial_status = self._agent.get_status()
        log.info(f"Session {self._session_id}: switched to agent '{template.name}'")

    async def aclose(self) -> None:
        """释放运行期资源（逐出路径专用）。

        顺序：先 ``shutdown()``（取消 worker，避免拆解期间还有调用方在用
        provider），再 ``aclose_providers()``（关闭该 agent 拥有的 client 表）。
        provider 的关闭放在 ``finally``：shutdown 失败（worker 带异常退出）
        也不能留下"已摘除但没拆干净"的 client——那时已没有人再持有它。

        只回收内存态，**不动磁盘**：消息日志已 append+fsync 落盘，元数据
        在该落的时候已落——session 仍可经 resume 完整水合回来。
        """
        try:
            await self._agent.shutdown()
        finally:
            await self._agent.aclose_providers()

    def apply_agent_override(self, override: AgentOverride) -> None:
        """应用 AgentOverride 到当前 session 的 agent（创建时调用）。

        在 from_template 之后调用，覆盖 template 中的特定字段。
        通过 agent 自身的公共方法完成覆盖，不直接操作内部状态。

        Override 语义：
        - None 字段不覆盖（保留 template 值）
        - system_prompt 替换，append_system_prompt 追加
        - 两者同时存在时，先替换再追加

        每个被应用的字段同步写入 metadata 并落盘——override 是显式动作，
        其效果必须跨重启（resume）与 fork 存活，否则系统提示词 / 工具集 /
        开关在重启后变回模板默认，请求前缀与重启前不一致（KV cache 碎裂）。
        """
        cm = self._context_manager
        agent = self._agent

        # 1. model 覆盖（若指定 provider 则切换 provider，否则用当前）
        #    走 _apply_model：与运行时切换同一条路径，一并落盘模型记录。
        if override.model is not None:
            self._apply_model(override.model, override.provider)

        # 2. system_prompt 替换（先替换，后追加，保证顺序正确）
        if override.system_prompt is not None:
            cm.setin_system_prompt = override.system_prompt
            self._record_state(system_prompt=override.system_prompt)

        # 3. append_system_prompt 追加（与 hook 注入内容合并进同一字段）
        if override.append_system_prompt is not None:
            cm.append_to_system_prompt(override.append_system_prompt)
            self._record_state(append_system_prompt=cm.append_system_prompt or None)

        # 4. tools 覆盖（从 registry 获取新的未绑定工具，避免闭包泄漏）
        if override.tools is not None:
            agent.set_tools(override.tools)
            self._record_state(tools=list(override.tools))

        # 5. max_turns 覆盖
        if override.max_turns is not None:
            agent.set_max_turns(override.max_turns)
            self._record_state(max_turns=override.max_turns)

        # 6. effort (reasoning_effort) 覆盖
        if override.effort is not None:
            agent.set_reasoning_effort(override.effort)
            self._record_state(reasoning_effort=override.effort)

        # 7. yolo 覆盖
        if override.yolo is not None:
            agent.set_yolo(override.yolo)
            self._record_state(yolo=override.yolo)

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
            provider_name=self._agent.model_provider.name,
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

    @property
    def persisted_thinking(self) -> bool | None:
        """记录在案的 thinking 开关（None = 无记录，跟随 provider 配置）。

        fork 快照（`SessionManager.fork_session`）取**记录**而非 live 派生值：
        派生默认（如 anthropic 未配置 thinking 时的 disabled）被固化进子会话
        记录后，会变成源会话请求体里不存在的显式配置——前缀身份被破坏，
        跨协议切模型时还会把一种协议的默认值贴到另一种协议上。
        """
        return self._metadata.thinking

    @property
    def persisted_reasoning_effort(self) -> str | None:
        """记录在案的推理力度（None = 无记录，跟随 provider 配置）。"""
        return self._metadata.reasoning_effort

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
            self._record_state(tools=list(tools))

        if title is not None:
            self.set_title(title)

        if thinking is not None:
            self.agent.model_provider.set_thinking(thinking)
            self._record_state(thinking=thinking)

        if reasoning_effort is not None:
            self.agent.set_reasoning_effort(reasoning_effort)
            self._record_state(reasoning_effort=reasoning_effort)

        if yolo is not None:
            self.agent.set_yolo(yolo)
            self._record_state(yolo=yolo)

        if workspace is not None:
            self.set_workspace(workspace)

    def _record_state(self, **fields: object) -> None:
        """把显式动作产生的会话状态写进 metadata 并落盘。

        保存的是整个 metadata 模型（幂等）；字段一律来自「用户显式动作 /
        创建 override / fork 快照」——绝不在普通请求路径上顺手快照，避免把
        模板/配置默认值固化成记录、挡住未来的配置变更。

        落盘是 best-effort（与 _persist_model 同口径）：live 状态已经生效，
        把一次磁盘写失败变成 500 只会制造新的前后端错位；最坏退化是本次
        进程内正确、重启后跟随模板/配置默认。
        """
        for name, value in fields.items():
            setattr(self._metadata, name, value)
        try:
            self._save_metadata()
        except OSError as e:
            log.warning(f"Session {self._session_id}: state not persisted ({e})")

    def reapply_provider_options(self) -> None:
        """把记录在案的 provider 级开关贴到**当前** provider 实例上（公开入口）。

        适用于 provider 实例被换掉的第三条路径：`/api/system/reload` 第 4 步
        对在场会话调 `agent.rebuild_providers()` 按新配置重建 provider——
        provider 级 extra_body 状态（thinking / reasoning_effort）随之归零，
        不重贴就会静默退回配置默认（请求前缀漂移，且与 metadata 记录失配，
        直到下次逐出 / resume 才被纠正）。
        """
        self._reapply_recorded_provider_options()

    def sync_tools_record(self) -> None:
        """把**当前生效**的工具集快照进 metadata 并落盘（fork 专用）。

        fork 记录的是源会话的 live refs，但按 ref 还原可能降级（远程工具宿主
        断连后 registry 里已无该 ref）：记录与 live 不一致会让子会话重启后
        tools 声明凭空变化——正是本次要消灭的"重建后前缀漂移"。fork 构造完
        子会话后调用本方法，把记录对齐到实际生效集合。
        """
        self._record_state(tools=tool_refs(self._agent.tools))

    def sync_append_system_prompt(self) -> None:
        """把 CM 当前 append_system_prompt 快照进 metadata 并落盘。

        create_session 在 before_session_start hook 跑完后调用——hook 注入的
        内容（如 workspace / OS 信息）是会话级状态，必须随创建落盘，resume /
        fork 重建 CM 时才能复现同一系统提示词（KV cache 前缀稳定）。
        与记录相同（含同为 None）时跳过，不产生写噪声。
        """
        value = self._context_manager.append_system_prompt or None
        if value == self._metadata.append_system_prompt:
            return
        self._record_state(append_system_prompt=value)

    def _apply_model(self, model: str, provider_name: str | None = None) -> None:
        """切换模型，必要时切换 provider——委托 agent 的单一持有能力。

        Session 不持有 provider：provider client 表归 WingAgent（按 name 有界
        持有、切回同名复用、跨 provider 切模型不关闭旧 client）。

        切换成功后把生效的 (provider, model) 记入 metadata 并落盘——这是
        显式模型动作的落盘点，也是模型选择跨进程重启的唯一恢复来源。

        跨 provider 切换会换上另一个 provider 实例（provider 级 extra_body
        状态归零）：记录在案的 thinking / reasoning_effort 重新贴回，避免
        一次 /model 就把会话开关悄悄改回配置默认（同 provider 切模型本就
        保留）。agent 级状态（yolo / max_turns）不随切换变化，不在此重贴。
        """
        self.agent.set_model(model, self._resolve_provider(provider_name))
        self._reapply_recorded_provider_options()
        self._persist_model()

    def _resolve_provider(self, provider_name: str | None) -> "ModelProvider":
        """按 name 解析 provider：None 或与当前同名时沿用当前活跃实例。"""
        if provider_name is None or provider_name == self.agent.model_provider.name:
            return self.agent.model_provider
        return self.agent.get_or_create_provider(provider_name)

    def _persist_model(self) -> None:
        """把 agent 当前生效的 (provider, model) 成对记入 metadata 并落盘。

        成对语义：不落盘半写记录（读取侧把单字段视为无记录）。
        保存的是整个 metadata，因此调用方（如 switch_template）设置的
        其他字段（template_name 等）随同一次写入落盘。

        落盘是 best-effort：写失败（disk full / 只读挂载 / 权限）只打 warning，
        不让 OSError 穿出去——切换已经生效，把请求变成 500 只会制造一次新的
        前后端错位（agent 在新模型上跑、前端以为失败）。最坏退化成本次进程内
        正确、重启后回模板默认。
        """
        self._metadata.model_name = self.agent.model
        self._metadata.provider_name = self.agent.model_provider.name
        try:
            self._save_metadata()
        except OSError as e:
            log.warning(
                f"Session {self._session_id}: model record not persisted ({e}); "
                "switch stays in effect for this process"
            )

    # ── 持久状态还原（构造时统一应用）─────────────

    def _restore_persisted_state(self) -> None:
        """把 metadata 记录的会话状态应用到 agent / CM（构造时统一调用）。

        覆盖 resume、fork 子会话构造、带 session_id 的磁盘恢复三条路径——
        「重建 agent = 从磁盘复现请求前缀」，这是 KV cache 跨重启存活的前提。
        全字段「记录存在才应用」：缺失视为无记录，跟随模板/配置默认，绝不
        把默认值固化成记录。

        顺序：提示词 → 工具 → 模型 → 动态开关。模型排在开关之前：跨 provider
        还原会换上 provider 实例，开关必须落在最终活跃的 provider 上。
        """
        self._restore_persisted_prompt()
        self._restore_persisted_tools()
        self._restore_persisted_model()
        # 模型还原可能换上别的 provider 实例：provider 级开关必须落在最终
        # 活跃的 provider 上；agent 级开关（yolo / max_turns）与 provider
        # 无关，单独应用（两者口径见各自 docstring）。
        self._reapply_recorded_provider_options()
        self._restore_persisted_agent_options()

    def _restore_persisted_prompt(self) -> None:
        """还原基础系统提示词替换与追加系统提示词。"""
        cm = self._context_manager
        if self._metadata.system_prompt is not None:
            cm.setin_system_prompt = self._metadata.system_prompt
        if self._metadata.append_system_prompt:
            cm.append_system_prompt = self._metadata.append_system_prompt

    def _restore_persisted_tools(self) -> None:
        """按记录还原可执行工具集（尽力而为）。

        ref 可能因远程宿主未连接 / 配置变更而失效：逐个解析、失效的跳过并
        告警；非空记录全部失效或绑定失败时保持模板工具集（宁可多给，不可
        裸奔——远程工具与动态工具切换尚无系统化设计，先按最简单语义处理）。

        `None`（无记录）与 `[]`（显式清空工具集）语义不同：后者是用户的
        显式动作，必须原样还原。
        """
        refs = self._metadata.tools
        if refs is None:
            return
        from wing.tool_registry import tool_registry

        resolved = [ref for ref in refs if tool_registry.resolve(ref) is not None]
        missing = [ref for ref in refs if ref not in resolved]
        if missing:
            log.warning(
                f"Session {self._session_id}: {len(missing)} recorded tool(s) "
                f"no longer resolvable, skipped: {missing}"
            )
        if refs and not resolved:
            log.warning(
                f"Session {self._session_id}: no recorded tool resolves; "
                "keeping template tools"
            )
            return
        # 还原 = 重建 agent：声明集复位为未初始化，set_tools 走 init 冷路径
        # （声明集直接跟随可执行集）——绝不触发热切换的 reminder 注入（那条
        # 消息在重启前的链里不存在，会碎掉请求前缀）。
        self._context_manager.reset_declared_tools()
        try:
            self._agent.set_tools(resolved)
        except ValueError as e:
            log.warning(
                f"Session {self._session_id}: cannot restore recorded tools ({e}); "
                "keeping template tools"
            )

    def _reapply_recorded_provider_options(self) -> None:
        """把记录在案的 **provider 级**开关应用到当前 provider（无记录即默认）。

        provider 实例在跨 provider 切换与模型还原时会被换掉（provider 级
        extra_body 状态归零），记录在案的 thinking（enable_thinking）与
        reasoning_effort 必须重新落上去。

        **只做 provider 级**：yolo / max_turns 是 agent 级状态，不随 provider
        切换变化——一并重贴会盖掉 live 值（例：Bash 工具的 "always allow"
        运行时打开 yolo，不产生任何记录）。
        """
        m = self._metadata
        if m.thinking is not None:
            self._agent.model_provider.set_thinking(m.thinking)
        if m.reasoning_effort is not None:
            self._agent.set_reasoning_effort(m.reasoning_effort)

    def _restore_persisted_agent_options(self) -> None:
        """还原 agent 级开关与限额（构造路径专用；无记录即跟随模板/配置）。"""
        m = self._metadata
        if m.yolo is not None:
            self._agent.set_yolo(m.yolo)
        if m.max_turns is not None:
            self._agent.set_max_turns(m.max_turns)

    def _restore_persisted_model(self) -> None:
        """从 metadata 还原模型绑定（重启后 resume 的核心动作）。

        - 记录存在（两字段齐全）时优先于模板默认模型，直接 set 到 agent；
          还原动作本身不落盘——记录已在磁盘上，不产生写噪声。
        - provider 已不可解析（config 变更/构建失败）时降级：打 warning、
          保持模板默认模型、记录原样保留（config 修复后下次 resume 仍可还原）。
        - 记录不完整（单字段）视为无记录。
        - model 不在 provider 的静态模型列表内时只打 warning，仍然还原
          （记录是用户选择，不因配置列表变动而作废）。
        """
        model = self._metadata.model_name
        provider_name = self._metadata.provider_name
        if model is None or provider_name is None:
            return
        try:
            provider_cfg = get_config().get_provider(provider_name)
            provider = self._resolve_provider(provider_name)
        except Exception as e:
            log.warning(
                f"Session {self._session_id}: cannot restore model '{model}' "
                f"on provider '{provider_name}' ({e}); "
                "falling back to template default (record kept)"
            )
            return
        # 静态模型列表非空时能对记录做一致性提示（不阻断）：记录到已下架
        # 模型时，用户看到的失败来自上游 model not found，看不出与 session
        # 记录有关——这条 warning 是唯一线索。列表为空 = 远端 /models 动态
        # 来源，跳过（避免在构造期发网络请求）。
        if provider_cfg.models and model not in provider_cfg.models:
            log.warning(
                f"Session {self._session_id}: recorded model '{model}' is not "
                f"in provider '{provider_name}' static model list; restoring anyway"
            )
        self.agent.set_model(model, provider)
        log.info(
            f"Session {self._session_id}: restored model "
            f"'{model}' (provider '{provider_name}')"
        )

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
