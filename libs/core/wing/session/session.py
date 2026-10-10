# wing/session/session.py
"""
wing/session/session.py — Session 类

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

from collections.abc import Iterable
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

from wing.chain import TrackedList
from wing.common.utils import require_utf8
from wing.common.logger import log
from wing.config import get_config
from wing.context import ContextManager
from wing.media import MediaAccess
from wing.schema import ChainNode, Message, Tool
from wing.store import SessionMetadata, SessionStore, TagMeta
from wing.tool_registry import ToolRef

from .model_binding import ModelBinding
from .override import validate_override_utf8
from .tags import TagMutation, apply_tag_ops, sanitize_tag_meta, sanitize_tags

if TYPE_CHECKING:
    from wing.agent import WingAgent
    from wing.event.base import AgentInfo, SessionStatus

    from .override import AgentOverride
    from .template import AgentTemplate


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
    if msg.media:
        # 引用式媒体（MediaRef 元数据），绝无 base64——字节在 SessionStore。
        d["media"] = [ref.model_dump() for ref in msg.media]
    return d


def tool_refs(tools: list[Tool]) -> list[str]:
    """把 Tool 列表投影为可持久化的 ref 列表（"Bash" / "client.Read"）。

    与 `WingAgent.set_tools()` 接受的引用格式一致——ref 可在任何进程经
    `tool_registry.resolve()` 还原；闭包 / dispatch 闭包不可持久化，这正是
    "只存引用不存对象"的原因。会话状态持久化（metadata.tools）与 fork 的
    工具集对齐共用本投影。
    """
    return [str(ToolRef(namespace=t.namespace, name=t.name)) for t in tools]


def validate_tool_refs(tools: list[str]) -> None:
    """纯校验工具 ref 可解析；不可解析即 raise ValueError。

    **先校验后动手**：一次覆盖里 model / 提示词 / 工具逐个应用，若工具排在后
    面才失败，前面的字段已经落盘（`_persist_model` 会写 metadata）——"部分
    应用"正是要避免的状态（尤其 create-or-adopt：失败的 create 不该留下任何
    残留）。所有覆盖入口（创建 / resume / update_state）都先过这里。
    """
    from wing.tool_registry import tool_registry

    for ref in tools:
        if tool_registry.resolve(ref) is None:
            raise ValueError(f"cannot resolve tool reference: '{ref}'")


def ignored_override_fields(override: "AgentOverride", *, resume: bool) -> list[str]:
    """本次覆盖里**不会生效**的字段名（纯函数，供 warning 与单测）。

    ``resume=True`` 时 ``system_prompt`` / ``append_system_prompt`` /
    ``max_turns`` / ``yolo`` 一律不生效（它们改请求前缀或会话既有限额，
    属创建期语义）。其余字段（``model_id`` / ``effort`` / ``tools``）两条
    路径都生效——provider 已不是覆盖字段（运行期事实，随 model_id 映射而来）。
    """
    ignored: list[str] = []
    if resume:
        ignored.extend(
            name
            for name, value in (
                ("system_prompt", override.system_prompt),
                ("append_system_prompt", override.append_system_prompt),
                ("max_turns", override.max_turns),
                ("yolo", override.yolo),
            )
            if value is not None
        )
    return ignored


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
        # 模型身份三元组的引用词（内存态）：id 是唯一引用词，provider/name 是
        # agent 持有的运行期事实。构造期由 _restore_persisted_model 填充；无
        # 记录时在下方按 live agent 反查兜底。
        self._model_id: str | None = None

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

        # model id 兜底（无记录 / 记录不可用回落模板默认时）：由 live agent 的
        # (provider, name) 反查补全——新会话因此从第一帧起就有引用词，/info 与
        # 状态事件不必等下一条显式动作。反查查不中即保持 None（不猜测）。
        if self._model_id is None:
            self._model_id = get_config().identify(
                self._agent.provider_name, self._agent.model
            )

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

        context_manager = ContextManager(
            session_id=session_id,
            messages=messages,
            system_prompt=template.system_prompt,
            compactor=template.compactor,
            skills_patterns=template.skills_patterns,
            rules_patterns=template.rules_patterns,
            workspace=workspace,
        )

        # provider name 先校验（不可解析 = 模板/配置错误，创建即失败）；
        # 实例经共享池解析（无状态化，见 wing.provider.pool）。
        get_config().get_provider(template.provider_name)
        # 会话媒体池：读/写都经窄接口，工具与 provider 序列化不直接触存储。
        media = MediaAccess(read=store.read_media, write=store.write_media)
        agent = WingAgent(
            model=template.model,
            provider_name=template.provider_name,
            stream=True,
            context_manager=context_manager,
            tools=template.resolved_tools,
            max_turns=template.max_turns,
            yolo=template.yolo,
            media=media,
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

        # 干净关闭旧 agent 的 worker；provider 实例归共享池，不随 agent 关闭
        # （其他会话可能正在用，且新 agent 多半解析到同一实例）。
        old_agent = self._agent
        await old_agent.shutdown()

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

        get_config().get_provider(template.provider_name)
        media = MediaAccess(read=self._store.read_media, write=self._store.write_media)
        self._agent = WingAgent(
            model=template.model,
            provider_name=template.provider_name,
            stream=True,
            context_manager=self._context_manager,
            tools=template.resolved_tools,
            max_turns=template.max_turns,
            yolo=template.yolo,
            media=media,
        )

        cwd = (
            str(Path(self._metadata.workspace).resolve())
            if self._metadata.workspace
            else None
        )
        self._agent.set_cwd(Path(cwd) if cwd else None)

        self._template_name = template.name
        self._metadata.template_name = template.name
        # 模型引用词：模板自带 id（fork 的 from_agent 传入源会话 id）；缺省按
        # 新模板的 (provider, name) 反查补全（模板来自配置，正常必命中）。
        self._model_id = template.model_id or get_config().identify(
            template.provider_name, template.model
        )
        # 模板切换以新模板为准——此前记录的显式覆盖（基础提示词 / 工具 /
        # 动态开关 / 限额）随切换作废：清记录即「跟随新模板与配置」的持久化
        # 表达（新 agent 已按新模板构造），避免重启后把旧覆盖又贴回新 agent。
        self._metadata.system_prompt = None
        self._metadata.tools = None
        self._metadata.thinking = None
        self._metadata.reasoning_effort = None
        self._metadata.yolo = None
        self._metadata.max_turns = None
        # 模板切换覆写模型记录（新模板的生效模型；_model_id 已在上面就位，
        # _persist_model 写三元组）；保存整个 metadata，与上面的清理同一次落盘。
        self._persist_model()
        log.info(f"Session {self._session_id}: switched to agent '{template.name}'")

    async def aclose(self) -> None:
        """释放运行期资源（逐出路径专用）。

        只关闭本会话的 worker；provider 实例归共享池（其他会话共用、逐出
        不拆池）——agent 只持有 name，没有任何 client 所有权要终结。

        只回收内存态，**不动磁盘**：消息日志已 append+fsync 落盘，元数据
        在该落的时候已落——session 仍可经 resume 完整水合回来。
        """
        await self._agent.shutdown()

    def apply_agent_override(self, override: AgentOverride) -> None:
        """应用 AgentOverride 到当前 session 的 agent（创建时调用）。

        在 from_template 之后调用，覆盖 template 中的特定字段。
        通过 agent 自身的公共方法完成覆盖，不直接操作内部状态。

        Override 语义：
        - None 字段不覆盖（保留 template 值）
        - system_prompt 替换，append_system_prompt 追加
        - 两者同时存在时，先替换再追加
        - ``model_id`` 是唯一的模型覆盖入口：单键查表（命中即切换调用名与
          provider），未命中 raise（C7 文案）——provider 随映射而来，不再单独下发

        每个被应用的字段同步写入 metadata 并落盘——override 是显式动作，
        其效果必须跨重启（resume）与 fork 存活，否则系统提示词 / 工具集 /
        开关在重启后变回模板默认，请求前缀与重启前不一致（KV cache 碎裂）。

        # 工具 ref 先做纯校验（失败时不落任何字段）；其它字段的应用不会失败。
        # 文本字段的可编码性同样在应用之前收口（非法 UTF-8 → 落盘就会炸）。
        被忽略的字段以 :func:`ignored_override_fields` 为统一判据——创建路径当前
        没有会被忽略的字段（``provider`` 已不是覆盖字段）。
        """
        cm = self._context_manager
        agent = self._agent

        validate_override_utf8(override)

        ignored = ignored_override_fields(override, resume=False)
        if ignored:
            log.warning(
                f"Session {self._session_id}: agent override ignores "
                f"{', '.join(ignored)} (create-time fields only)"
            )

        if override.tools is not None:
            validate_tool_refs(override.tools)

        # model_id 覆盖走 _apply_model：与运行时切换同一条路径，一并落盘模型记录。
        if override.model_id is not None:
            self._apply_model(override.model_id)

        if override.system_prompt is not None:
            cm.setin_system_prompt = override.system_prompt
            self._record_state(system_prompt=override.system_prompt)

        if override.append_system_prompt is not None:
            cm.append_to_system_prompt(override.append_system_prompt)
            self._record_state(append_system_prompt=cm.append_system_prompt or None)

        if override.tools is not None:
            agent.set_tools(override.tools)
            self._record_state(tools=list(override.tools))

        if override.max_turns is not None:
            agent.set_max_turns(override.max_turns)
            self._record_state(max_turns=override.max_turns)

        if override.effort is not None:
            agent.set_reasoning_effort(override.effort)
            self._record_state(reasoning_effort=override.effort)

        if override.yolo is not None:
            agent.set_yolo(override.yolo)
            self._record_state(yolo=override.yolo)

        log.info(
            f"Session {self._session_id}: applied agent override "
            f"(model_id={override.model_id}, "
            f"tools={override.tools}, "
            f"max_turns={override.max_turns}, effort={override.effort}, "
            f"yolo={override.yolo})"
        )

    def apply_resume_override(self, override: AgentOverride) -> None:
        """应用 **resume 语义** 的 AgentOverride 子集（恢复既有会话时调用）。

        只应用 ``model_id`` / ``effort`` / ``tools``——它们改的是
        "下一轮怎么发起请求"，不触碰已落链的对话内容。其余字段**一律不应用**：

        - ``system_prompt`` / ``append_system_prompt``：改的是系统提示词，
          即请求前缀的第一段——既有会话的链是按老前缀建立的，中途换掉会让
          前缀身份漂移（KV cache 碎裂）；要换请对新会话用创建覆盖，或走
          `session/update` 的显式动作；
        - ``max_turns``：会话既有限额是运行时状态，不因"续链"被改写；
        - ``yolo``：同上（创建期决定，resume 不重贴）。

        给了被忽略的字段会打 warning（**不做静默忽略**：编排方能在日志里看到
        `--system-prompt` 在 `-r` 下没生效），但仍然继续应用子集。

        每个被应用的字段同步写入 metadata 并落盘（与创建覆盖同一套语义），
        因此跨重启 / 逐出后水合仍然生效。
        """
        ignored = ignored_override_fields(override, resume=True)
        if ignored:
            log.warning(
                f"Session {self._session_id}: resume override ignores "
                f"{', '.join(ignored)} (prompt fields would change the "
                "conversation prefix; max_turns / yolo are create-time session "
                "settings)"
            )

        # 纯校验在前（工具 ref 不可解析时不得留下"model 已切换"的半截状态）
        validate_override_utf8(override)
        if override.tools is not None:
            validate_tool_refs(override.tools)

        if override.model_id is not None:
            self._apply_model(override.model_id)

        if override.tools is not None:
            self._agent.set_tools(override.tools)
            self._record_state(tools=list(override.tools))

        if override.effort is not None:
            self._agent.set_reasoning_effort(override.effort)
            self._record_state(reasoning_effort=override.effort)

        log.info(
            f"Session {self._session_id}: applied resume override "
            f"(model_id={override.model_id}, "
            f"tools={override.tools}, effort={override.effort})"
        )

    # ── 暴露属性 ──────────────────────────────────

    @property
    def agent(self) -> "WingAgent":
        return self._agent

    @property
    def model_id(self) -> str | None:
        """当前模型的引用词（∈ 配置声明的 id 空间；不可用时 None）。

        三元组的唯一对外读口：/info、AgentInfo、状态事件、fork 快照都取它。
        provider_name / model_name（运行期事实）从 ``self.agent`` 读。
        """
        return self._model_id

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
    def session_name(self) -> str | None:
        """当前 session 名称（来自 metadata）。"""
        return self._metadata.session_name

    @property
    def session_workspace(self) -> str | None:
        """当前 session 的工作目录（来自 metadata）。"""
        return self._metadata.workspace

    @property
    def tags(self) -> list[str]:
        """当前会话标签（metadata.tags 的投影；插入序，读侧已清洗）。

        磁盘载入的存量数据可能被手改 / 老版本写入：投影前经 ``sanitize_tags``
        去重 + 丢弃违规格值（绝不 raise）——展示面永远合法。
        """
        return sanitize_tags(self._metadata.tags)

    @property
    def tag_meta(self) -> dict[str, TagMeta]:
        """当前会话的标签记录（打标时间等；键集 ⊆ ``tags``，读侧已清洗）。"""
        return sanitize_tag_meta(self._metadata.tag_meta, self.tags)

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
            ValueError: 路径不可编码为 UTF-8 / 不存在 / 不是目录
        """
        require_utf8(path, field="workspace")
        resolved = Path(path).expanduser().resolve()
        if not resolved.exists():
            raise ValueError(f"workspace path does not exist: {resolved}")
        if not resolved.is_dir():
            raise ValueError(f"workspace path is not a directory: {resolved}")

        resolved_str = str(resolved)
        self._metadata.workspace = resolved_str
        self._agent.set_cwd(resolved)
        self._context_manager.set_workspace(resolved)
        self._save_metadata()
        log.info(f"Session {self._session_id}: workspace changed to {resolved_str}")

    # ── 序列化方法 ──────────────────────────────────

    def model_binding(self) -> ModelBinding:
        """当前生效的模型绑定（引用词 + 运行期事实 + 展示名，同源同刻）。

        与 :meth:`to_agent_info` 的模型四件套同源：``model_id`` 来自内存态三元组
        （构造期恢复或显式动作写入），其余三项来自 live agent。**记录面**的同义
        投影是 :func:`wing.session.model_binding.resolve_model_binding`（列表里的
        未加载会话用那条路）——两处的答案必须是同一个模型。

        不抛：agent 侧的三项都是状态读取（展示名在 provider 不可解析时降级为
        None，见 ``WingAgent.model_display_name``）——列表是跨会话视图，不能被
        单个会话的配置问题拖垮。
        """
        return ModelBinding(
            model_id=self._model_id,
            model_name=self._agent.model,
            provider_name=self._agent.provider_name,
            model_display_name=self._agent.model_display_name,
        )

    def to_agent_info(self) -> "AgentInfo":
        """从 Session 的 agent 和 context_manager 构造 AgentInfo。

        模型四件套走 :meth:`model_binding`（与列表条目同一次取法）：四处出口
        （info / get / sync 重放 / 列表）说的必须是同一个模型。
        """
        from wing.event.base import AgentInfo

        cm = self._context_manager
        binding = self.model_binding()
        return AgentInfo(
            model_name=binding.model_name,
            model_id=binding.model_id,
            system_prompt=cm.system_prompt.content if cm.system_prompt else None,
            tools=[t.effective_llm_name for t in self._agent.tools],
            skills=list(cm._skills_cache.keys()),
            rules=list(cm._rules_files),
            workspace=self._metadata.workspace,
            provider_name=binding.provider_name,
            model_display_name=binding.model_display_name,
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
        """设置 session 标题并持久化。

        Raises:
            ValueError: 标题不可编码为 UTF-8（落盘就会炸，且失败路径不留半份记录）
        """
        require_utf8(title, field="session title")
        self._metadata.session_name = title
        self._save_metadata()

    def apply_tag_ops(
        self, *, add: Iterable[str] = (), remove: Iterable[str] = ()
    ) -> TagMutation:
        """原子应用标签增删并落盘（幂等；无实际变化不产生写）。

        打标时间随变更一并维护（新增记时间、移除删记录）——整条 metadata
        一次落盘，两个字段不会各自漂移。

        Raises:
            ValueError: 标签非法（校验在 ``session.tags`` 统一执行）
        """
        mutation = apply_tag_ops(
            self._metadata.tags,
            add=add,
            remove=remove,
            meta=self._metadata.tag_meta,
        )
        if mutation.added or mutation.removed:
            self._metadata.tags = mutation.tags or None
            self._metadata.tag_meta = mutation.tag_meta or None
            self._save_metadata()
        return mutation

    async def update_state(
        self,
        *,
        model_id: str | None = None,
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
            model_id: 切换模型（引用词 = providers[].models 的 id；未命中 raise）
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
            validate_tool_refs(tools)
        # 文本字段的可编码性：model_id / effort 先写内存态、再落 metadata 与
        # LLM 请求体——非 UTF-8（孤立代理字符）会让 `_persist_model` 在
        # `encode("utf-8")` 处抛错，而这时**内存态已经被污染**（此后 `/info` 序列化
        # 就炸、该会话所有写操作全失败）。与 set_title / set_workspace 同形：拦在
        # mutation 之前，不可编码的输入等价于"没发生过"。
        if model_id is not None:
            require_utf8(model_id, field="model_id")
        if reasoning_effort is not None:
            require_utf8(reasoning_effort, field="reasoning_effort")
        # id 查表同样前置（`_apply_model` 内仍会查一次，二次查表无副作用）：
        # mutation 是**有序**的（template → model → …），未命中的 model_id 必须与
        # 其它非法字段同价——否则 `{agent: X, model_id: <未知>}` 会先切模板、落盘
        # 三元组，再在 `_apply_model` 处 raise：错误响应 + 已生效的变更同时出现，
        # 请求级留下半截状态（"任何字段非法在 mutation 之前退出"因此破功）。
        if model_id is not None:
            get_config().require_model(model_id)

        if template is not None:
            await self.switch_template(template)

        if model_id is not None:
            self._apply_model(model_id)

        if tools is not None:
            self.agent.set_tools(tools)
            self._record_state(tools=list(tools))

        if title is not None:
            self.set_title(title)

        if thinking is not None:
            self.agent.set_thinking(thinking)
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
        进程内正确、重启后跟随模板/配置默认。**不可编码为 UTF-8 的值**
        （hook 注入的文本 / 配置里的非法转义）与"写不进去"同价：写盘必然
        失败，没有重试余地。
        """
        for name, value in fields.items():
            setattr(self._metadata, name, value)
        try:
            self._save_metadata()
        except (OSError, UnicodeEncodeError) as e:
            log.warning(f"Session {self._session_id}: state not persisted ({e})")

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

    def _apply_model(self, model_id: str) -> None:
        """按引用词切换模型（含 provider 随映射而来的切换）。

        单键查表：命中即 `agent.set_model(ref.name, ref.provider_name)` 并记下
        `_model_id`；**未命中 raise**（C7 文案，`Config.require_model`）——先查
        后改，状态零变化。Session 与 agent 都不持有 provider 实例：实例归共享池，
        agent 只记 name（切回同名解析到同一实例，无创建 / 关闭动作）。

        切换成功后把生效的 (model_id, provider, name) 三元组记入 metadata 并
        落盘——这是显式模型动作的落盘点，也是模型选择跨进程重启的唯一恢复来源。

        会话级开关（thinking / reasoning_effort）住 agent、与 provider 实例
        无关，跨 provider 切换天然保留（不存在实例更替导致的状态归零）；
        agent 级状态（yolo / max_turns）同样不随切换变化。
        """
        ref = get_config().require_model(model_id)
        self.agent.set_model(ref.name, ref.provider_name)
        self._model_id = ref.id
        self._persist_model()

    def _persist_model(self) -> None:
        """把模型身份三元组 (model_id, provider_name, model_name) 记入 metadata 并落盘。

        三元组一次写全（不落半写记录：读取侧把 id 缺失但有快照的记录视为旧记录，
        恢复时经 `Config.identify` 反查补 id）。model_id 是引用词，provider_name /
        model_name 是此刻的运行期事实快照。保存的是整个 metadata，因此调用方
        （如 switch_template）设置的其他字段（template_name 等）随同一次写入落盘。

        落盘是 best-effort：写失败（disk full / 只读挂载 / 权限）只打 warning，
        不让 OSError 穿出去——切换已经生效，把请求变成 500 只会制造一次新的
        前后端错位（agent 在新模型上跑、前端以为失败）。最坏退化成本次进程内
        正确、重启后回模板默认。不可编码为 UTF-8 的模型名（配置里的非法转义 /
        远端工具宿主注册的名字）同样是"写不进去"，一并按 best-effort 处理。
        """
        self._metadata.model_id = self._model_id
        self._metadata.model_name = self.agent.model
        self._metadata.provider_name = self.agent.provider_name
        try:
            self._save_metadata()
        except (OSError, UnicodeEncodeError) as e:
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

        顺序：提示词 → 工具 → 模型 → 动态开关。模型排在开关之前：请求首次
        发起前所有状态就位（开关住 agent、与 provider 实例无关，顺序不敏感，
        保持"模型先行"以对齐历史语义）。
        """
        self._restore_persisted_prompt()
        self._restore_persisted_tools()
        self._restore_persisted_model()
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

    def _restore_persisted_agent_options(self) -> None:
        """还原 agent 级开关与限额（构造路径专用；无记录即跟随模板/配置）。

        thinking / reasoning_effort 现在也是 agent 级状态（provider 无状态化：
        实例不再承载会话开关），与 yolo / max_turns 同口径——「记录存在才应用，
        缺失跟随模板/配置默认」，绝不把派生默认固化成记录。
        """
        m = self._metadata
        if m.thinking is not None:
            self._agent.set_thinking(m.thinking)
        if m.reasoning_effort is not None:
            self._agent.set_reasoning_effort(m.reasoning_effort)
        if m.yolo is not None:
            self._agent.set_yolo(m.yolo)
        if m.max_turns is not None:
            self._agent.set_max_turns(m.max_turns)

    def _restore_persisted_model(self) -> None:
        """从 metadata 还原模型身份三元组（重启后 resume 的核心动作）。

        恢复链（优先级固定，**无候选集合 / 无优先级回落 / 不猜测**）：

        1. ``model_id`` 命中 id 空间 → **用当前映射**（``ref.name`` /
           ``ref.provider_name``，跟随配置演化；id 不变而 name/provider 变了
           是配置作者的正常操作），``_model_id = ref.id``；
        2. ``model_id`` 未命中（id 被删）→ 用记录里的**快照** ``(provider_name,
           model_name)`` 继续跑 + warning；``_model_id = identify(provider, name)``
           （反查补全，可能 None——name 也不在声明里时不编 id）；
        3. 快照 provider 不可解析（config 变更 / 构建失败）→ 回落模板默认 +
           warning，**记录原样保留**（config 修复后下次 resume 仍可还原）；
        4. 旧记录（无 ``model_id``）→ 同 2 的快照路径 + ``identify`` 反查补 id；
        5. 记录不完整（只有 id 或只有半边快照）：id 能命中走 1，否则视为无记录。

        ``identify`` 是**单键反查**（同 provider 内调用名唯一 ⇒ 至多一个命中），
        只回答「这个 (provider, name) 对应哪个 id」，不参与「该跑哪个模型」的决策。

        对齐落盘（一次性迁移，不是写噪声）：只有当内存三元组与记录**不一致、
        且拿到了新事实**（id 命中后的快照跟随 / 旧记录补 id）才写回 metadata。
        id 未命中且反查也未命中时**不写**——旧 id 与快照是唯一恢复线索，擦掉
        等于销毁信息（与第 3 级的"记录保留"同一口径）。
        """
        m = self._metadata
        config = get_config()

        restored = False
        if m.model_id is not None:
            ref = config.find_model(m.model_id)
            if ref is not None:
                # ① id 命中：用当前映射（name / provider 跟随配置演化）。
                self.agent.set_model(ref.name, ref.provider_name)
                self._model_id = ref.id
                restored = True
                log.info(
                    f"Session {self._session_id}: restored model id "
                    f"'{ref.id}' → '{ref.name}' (provider '{ref.provider_name}')"
                )

        if not restored and m.model_name is not None and m.provider_name is not None:
            try:
                config.get_provider(m.provider_name)
            except ValueError as e:
                # ③ 快照 provider 不可解析：回落模板默认，记录保留。
                log.warning(
                    f"Session {self._session_id}: cannot restore model "
                    f"'{m.model_name}' on provider '{m.provider_name}' ({e}); "
                    "falling back to template default (record kept)"
                )
                return
            # ②/④ 快照兜底（id 被删 / 旧记录无 model_id）：记录是用户选择，
            # 不因配置列表变动而作废——仍然还原，只做反查补 id。
            self.agent.set_model(m.model_name, m.provider_name)
            self._model_id = config.identify(m.provider_name, m.model_name)
            restored = True
            if m.model_id is not None:
                log.warning(
                    f"Session {self._session_id}: recorded model id "
                    f"'{m.model_id}' is not in config; restoring recorded "
                    f"snapshot '{m.model_name}' (provider '{m.provider_name}')"
                )
            if self._model_id is None:
                log.warning(
                    f"Session {self._session_id}: recorded model "
                    f"'{m.model_name}' is not declared by provider "
                    f"'{m.provider_name}' (no model id backfilled); "
                    "restoring anyway"
                )
            log.info(
                f"Session {self._session_id}: restored model '{m.model_name}' "
                f"(provider '{m.provider_name}')"
                + (
                    f"; backfilled model id '{self._model_id}'"
                    if self._model_id is not None
                    else "; no id backfilled"
                )
            )

        if not restored:
            return

        # 对齐落盘（一次性迁移）：内存三元组 ≠ 记录才写，且只在拿到新事实
        # （id 或快照跟随映射）时写——见 docstring。
        if self._model_id is not None and (
            self._model_id,
            self.agent.provider_name,
            self.agent.model,
        ) != (m.model_id, m.provider_name, m.model_name):
            log.info(
                f"Session {self._session_id}: aligning persisted model record to "
                f"(id='{self._model_id}', provider='{self.agent.provider_name}', "
                f"name='{self.agent.model}')"
            )
            self._persist_model()

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
            return
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

        Raises:
            ValueError: 正文不可编码为 UTF-8（它在首条消息时会成为标题，也会进
                LLM 请求体——两处都在 ``encode("utf-8")`` 处炸）
        """
        require_utf8(content, field="message content")
        self._check_first_message_metadata(content)
        self.touch_last_interaction()
        await self._agent.post(
            content, request_id=request_id, tool_call_id=tool_call_id
        )
