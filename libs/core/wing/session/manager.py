# wing/session/manager.py
"""
wing/session/manager.py — SessionManager

管理 session 生命周期、消息路由、魔术命令分发。

核心设计约束：
  - SessionManager 只处理跨 session 行为（create、resume、switch、fork、列表；id 均为精确匹配）
  - 单 session 内部逻辑（metadata 管理、title 设置）在 Session 中
  - 持久状态统一经由 SessionStore——SM 不直接与存储介质打交道
  - WingAgent 不知道 SessionManager 的存在——它通过 EventBus 投递事件。
  - EventBus 不知道 WingAgent 的存在——它只是路由管道。
  - 路由表由 WingRuntime 统一管理，SM 不感知 client_id 和 contextvars。
  - _post() 采用两级分发：/ 开头走 magic_registry，否则走 session.post()
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Iterable
from uuid import uuid4
from datetime import datetime
from typing import TYPE_CHECKING

from wing.chain import TrackedList
from wing.common.logger import log
from wing.common.utils import (
    generate_session_id,
    is_valid_session_id,
    validate_session_id,
)
from wing.config import get_config
from wing.hooks import hooks
from wing.event import (
    DeliveredEvent,
    EventTarget,
    SessionInfo,
)
from wing.event_bus import event_bus
from wing.commands import expand_prompt_command
from wing.schema import ChainNode, Message
from wing.store import SessionMetadata, SessionStore

from .session import Session, tool_refs
from .tags import TagMutation, apply_tag_ops, sanitize_tag_meta, sanitize_tags
from .template import AgentTemplate, AgentTemplateManager

if TYPE_CHECKING:
    from .override import AgentOverride


def _fork_slice(
    records: Iterable[dict],
    target_uuid: str,
) -> tuple[list[dict], str | None]:
    """切出 fork 的记录前缀（**append 顺序**）与 draft。

    - ``"current"`` → 全部记录，draft 为空串；
    - 其他 → 目标记录**之前**的全部记录（目标自身不进拷贝：它由响应里的
      draft 重新发送），draft = 目标记录的 content。

    用记录口径（而不是链遍历）是刻意的：被压缩区间的记录原样保留在子会话里
    （用户仍可回退 / 分叉到压缩前的 User Message），而"活跃链从哪里开始"由
    压缩节点自身编码（``parent_uuid=None`` + ``unzip_last_uuid``）——子会话
    加载时沿 parent_uuid 回溯到该节点即止，已摘要内容不会复活。选压缩前的
    节点时，压缩节点是后来追加的记录，切在前缀之外 → 子会话里压缩仿佛没发生
    过（等价于"在压缩点之前分叉"）。

    ``records`` 是**迭代器**：命中目标即停，fork 点之后的记录不读也不解析
    ——只有 ``"current"`` 需要走到末尾（全部记录都是前缀）。

    Raises:
        ValueError: 目标 uuid 不在记录里，或指向的是事件记录（事件不是对话节点）。
    """
    if target_uuid == "current":
        return list(records), ""
    prefix: list[dict] = []
    for record in records:
        if record.get("uuid") == target_uuid:
            if record.get("role") == "event":
                raise ValueError(
                    f"uuid {target_uuid!r} is an event record, not a Message"
                )
            content = record.get("content")
            return prefix, content if isinstance(content, str) else ""
        prefix.append(record)
    raise ValueError(f"uuid {target_uuid!r} not found in {len(prefix)} record(s)")


def _timestamp_key(s: SessionInfo) -> float:
    """归一化 session 的「最后一次交互时间」为可比较的 epoch 秒。

    「有啥用啥」的三级回退：`metadata.last_interaction`（ISO 字符串或数值）→
    缺失 / 不可解析时用 session id 前缀（`YYYYMMDD-HHMMSS`）→ 都没有按 0。
    永不抛：排序键不可解析时退化成「排最后」，而不是让整个列表 500。

    时区口径：naive 的 ISO 字符串（仓库内唯一实际写法，`datetime.now().isoformat()`）
    与 id 前缀回退按**宿主本地时区**折算 epoch，带 `Z` / `+00:00` 的 aware 字符串按
    UTC 折算——同一份列表里不要混写两种字符串，否则按本地 UTC 偏移错序（小时级）。
    数值分支是防御代码（`SessionMetadata.last_interaction` 只声明 `str`）。
    """
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


def _remap_record_uuids(records: list[dict]) -> list[dict]:
    """复制记录并把链拓扑 uuid 全量重映射到子会话的 uuid 空间。

    重映射 ``uuid`` / ``parent_uuid`` / ``unzip_last_uuid`` 三个键。前缀口径下
    所有引用都指向前缀内部（parent 必然更早创建、unzip 指向区间末），因此
    重映射后子记录集自洽：不留悬空引用，也不需要清空任何引用——压缩节点的
    unzip 指向重映射后的区间末记录，子会话的 fork 候选列表与源会话一致。

    缺省落到 None 是防御：万一出现前缀外的引用，宁可让它成为根节点，也不留
    跨 session 的引用。

    只做**顶层浅拷贝**：重写的三个键都在顶层，而记录（含嵌套值）对读取方
    一律只读（``MessageLog`` 的记录契约：file 后端每次重新解析，memory
    后端直接交出内部结构，两边都没有就地改写记录的路径），深拷贝没有额外
    保护面。
    """
    clones = [{**record} for record in records]
    uuid_map: dict[str, str] = {}
    for clone in clones:
        uuid = clone.get("uuid")
        if isinstance(uuid, str):
            uuid_map[uuid] = str(uuid4())
    for clone in clones:
        for key in ("uuid", "parent_uuid", "unzip_last_uuid"):
            value = clone.get(key)
            if isinstance(value, str):
                clone[key] = uuid_map.get(value)
    return clones


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

        # 逐出（eviction）簿记：空闲计时器 + 在途拆解任务。
        # `_last_active` 是纯内存管理数据（不是持久状态）——任何会话状态
        # 变化（事件/操作）都会 touch 刷新；摘除时随会话一起丢弃。
        self._last_active: dict[str, float] = {}
        self._teardowns: set[asyncio.Task[None]] = set()

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
        workspace: str | None = None,
        agent_override: AgentOverride | None = None,
        backend: str | None = None,
        tags: Iterable[str] | None = None,
        session_id: str | None = None,
    ) -> Session:
        """创建（或按 id 收养）session。

        ``session_id`` 是 **create-or-adopt** 入口（编排方自带 id 的场景，如
        Claude Agent SDK 系消费方用自己的 UUID 建会话）：给定时该 id 即最终
        session id——已存在（内存或任 store）则**收养**既有会话，语义等同
        :meth:`resume_session`（模板 / workspace 来自 metadata，agent 覆盖只
        应用 resume 子集，见 :meth:`Session.apply_resume_override`），且不触发
        `before_session_start`（session id 未变，是"恢复"而非"创建新会话"）；
        不存在则以该 id 建会话（此时 agent 覆盖是创建语义：全字段应用）。

        不给 ``session_id`` 时行为不变：id 由后端生成（见
        :meth:`_generate_session_id`）——**既有策略是默认，不是唯一**。

        Args:
            template_name: Agent 模板名称，None 时使用默认模板（仅新会话生效）
            workspace: 工作目录（仅新会话生效）
            agent_override: AgentOverride 参数覆盖（None 字段不覆盖 template 值；
                收养路径只应用 model/provider/effort/tools）
            backend: 存储后端名称（如 file/memory），None 时使用默认后端
            tags: 创建即打标 / 收养时并入（经 :meth:`set_session_tags` 同一套校验）
            session_id: 指定 session id（create-or-adopt）；None = 自生成

        Raises:
            ValueError: session_id 不合规 / 模板不存在 / backend 未知 / 标签非法
        """
        # 闸门先于一切：不合规的 id 不得触达任何 store（防路径穿越），也必须
        # 在任何写盘之前失败——否则一个失败的 create 会在磁盘上留下半份
        # metadata，下一次同 id 的 create 就会"收养"一个幽灵会话。
        if session_id is not None:
            validate_session_id(session_id)
            adopted = self._adopt_session(
                session_id, agent_override=agent_override, tags=tags
            )
            if adopted is not None:
                return adopted

        backend_name = backend if backend is not None else self._default_backend
        store = self._stores.get(backend_name)
        if store is None:
            raise ValueError(
                f"Unknown storage backend '{backend_name}'. "
                f"Available: {list(self._stores)}"
            )

        # 标签纯校验提到最前（不写盘）：任何非法标签在任何副作用之前 raise，
        # "失败即零残留"对 create-or-adopt 尤其重要（重试必须还是干净状态）。
        if tags:
            apply_tag_ops([], add=tags)

        if template_name is not None:
            template = self._template_manager.get(template_name)
            if template is None:
                raise ValueError(
                    f"Agent 模板 '{template_name}' 不存在。"
                    f"可用模板: {self._template_manager.all_names}"
                )
        else:
            template = self._template_manager.default

        # 指定 id 时它就是最终 id；否则自生成（默认策略）
        sid = session_id if session_id is not None else self._generate_session_id()

        messages: TrackedList[ChainNode] = TrackedList(store.open_log(sid))

        session = Session.from_template(
            template=template,
            session_id=sid,
            messages=messages,
            store=store,
            workspace=workspace,
        )

        # agent override 需在 session 完全构建后应用
        if agent_override is not None:
            session.apply_agent_override(agent_override)

        # 创建即打标（原子语义：校验在写盘前完成，非法标签此刻 raise——
        # 会话尚未注册、磁盘零痕迹）
        if tags:
            session.apply_tag_ops(add=tags)

        self._sessions[sid] = session
        self.touch(sid)

        # 触发 before_session_start hook，随后把 hook 注入落盘：resume 重建 CM
        # 时恢复同一系统提示词（前缀身份不因换入内存而漂移）。
        hooks.invoke("before_session_start", session)
        session.sync_append_system_prompt()

        log.info(f"Session created: {sid} (template={template.name})")
        return session

    # ── resolve ───────────────────────────────

    def _resolve_with_store(self, session_id: str) -> tuple[str, SessionStore] | None:
        """跨 stores 精确解析 session id，返回 (session_id, store)。

        优先命中内存中的 session，再按 stores 注册顺序查后端是否存在。

        **闸门在最前**：session id 由会话层确定（默认自生成，编排方可自带，
        见 ``common.utils.is_valid_session_id``），不合规的值一律按"不存在"
        处理——绝不允许它进入任何 store 调用（file 后端拿它拼路径，这是路径
        穿越的唯一入口；gate 在这里，所有网络路径都经过本方法）。解析失败
        与闸门拒绝最终都映射为 404，不向客户端区分（不给探测反馈）。
        """
        if not is_valid_session_id(session_id):
            return None
        if session_id in self._sessions:
            return session_id, self._sessions[session_id].store
        for store in self._stores.values():
            if store.exists(session_id):
                return session_id, store
        return None

    def resume_session(
        self,
        session_id: str,
        agent_override: "AgentOverride | None" = None,
    ) -> Session:
        """恢复已有 session（精确匹配 session id）。已在内存中则直接返回。

        模板只从 metadata.template_name 解析——resume 不接受显式模板：
        **metadata 是模板的唯一来源**，要换模板请在恢复后走
        `session/update`（agent 字段）。template_name 缺失或已不存在于
        config 时回退默认模板。

        ``agent_override`` 是 resume 语义的参数覆盖（编排方 `--model` 等）：
        只应用 `model` / `provider` / `effort` / `tools` 子集——见
        :meth:`Session.apply_resume_override`（不改链上前缀是不变量）。

        Args:
            session_id: 目标 session ID（须为完整 ID）
            agent_override: 恢复后应用的参数覆盖（None = 不覆盖）

        Returns:
            恢复后的 Session 实例

        Raises:
            LookupError: session 不存在
            ValueError: 覆盖里的工具引用无法解析
        """
        result = self._resolve_with_store(session_id)
        if result is None:
            raise LookupError(f"Session not found: {session_id}")
        resolved, store = result

        existing = self._sessions.get(resolved)
        if existing is not None:
            # 已在内存的早退路径同样要应用覆盖——否则"刚被逐出的会话 resume
            # 时覆盖生效、未逐出的会话覆盖丢失"会成为一个静默分叉。
            if agent_override is not None:
                existing.apply_resume_override(agent_override)
            return existing

        metadata = store.load_metadata(resolved)

        # metadata 是模板的唯一来源：template_name > 默认
        tpl = None
        if metadata is not None and metadata.template_name is not None:
            tpl = self._template_manager.get(metadata.template_name)
        if tpl is None:
            tpl = self._template_manager.default

        messages: TrackedList[ChainNode] = TrackedList.load(
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
        if agent_override is not None:
            session.apply_resume_override(agent_override)
        self._sessions[resolved] = session
        self.touch(resolved)
        log.info(f"Session resumed: {resolved}")
        return session

    def _adopt_session(
        self,
        session_id: str,
        *,
        agent_override: "AgentOverride | None",
        tags: Iterable[str] | None,
    ) -> Session | None:
        """create-or-adopt 的「已存在」分支：收养既有会话。

        语义 = :meth:`resume_session` + 覆盖子集 + tags 并入：
        - 模板 / workspace 来自 metadata（"创建"参数对既有会话无意义）；
        - `agent_override` 走 resume 子集（model/provider/effort/tools）；
        - `tags` 按 add 语义并入（幂等；非法标签在此 ValueError，零写盘）；
        - **不触发 `before_session_start`**：session id 未变，这是"恢复既有
          会话"而非"创建新会话"（该 hook 的语义边界就是"新 session id"）。

        Returns:
            收养到的 Session；``session_id`` 不存在时返回 None（调用方走新建）。
        """
        if self._resolve_with_store(session_id) is None:
            return None
        # 标签先纯校验：非法标签不得留下"覆盖已应用"的半截状态。
        if tags:
            apply_tag_ops([], add=tags)
        session = self.resume_session(session_id, agent_override=agent_override)
        if tags:
            session.apply_tag_ops(add=tags)
        log.info(f"Session adopted: {session_id} (create-or-adopt)")
        return session

    def fork_session(
        self,
        session_id: str,
        target_uuid: str,
    ) -> tuple[Session, str | None] | None:
        """从指定 session 的 target_uuid 处 fork 出新 session。

        **子会话 = 源会话记录的前缀**（append 顺序，见 ``_fork_slice``）：源
        记录的 uuid 全量重映射后写进子会话自己的日志，再用加载路径构造内存态
        ——"子会话在内存里就长得像重启后加载出来的样子"，活跃链由 tip 回溯
        自然得出。被压缩区间的记录随行走（用户仍可回到压缩前的 User Message），
        但不会进入活跃链。

        元数据一次写全（workspace / forked_from / template_name / 模型快照 /
        系统提示词与动态状态快照 / last_interaction）；消息与元数据均经由源
        session 所属 store 写入——fork 的正确性由 store 单一所有者保证。
        """
        source = self._sessions.get(session_id)
        if source is None:
            return None

        store = source.store
        new_session_id = self._generate_session_id()

        # 记录前缀切片（append 顺序）+ uuid 重映射，写进子会话自己的日志
        try:
            copied, draft = _fork_slice(
                store.open_log(session_id).iter_all(), target_uuid
            )
        except ValueError:
            log.warning(f"fork_session: uuid {target_uuid} not found")
            return None
        child_log = store.open_log(new_session_id)
        child_log.append(_remap_record_uuids(copied))

        # 用加载路径构造内存态（同 resume）：活跃链 = tip 回溯，压缩区间不进链
        new_messages: TrackedList[ChainNode] = TrackedList.load(child_log, Message)

        # 一次写全元数据——模型 / 提示词 / 工具集 / yolo / max_turns 都是**快照**：
        # 子会话的 agent 由源 agent 反向抽取模板构造（生效模型 = 源此刻模型），
        # metadata 记录同一对值，重启后 resume 才不会偏离 fork 时用户看到的值。
        # 注意 fork 不承诺与源会话的前缀身份：session id 变化 +
        # before_session_start 在新会话上重跑（见下）。口径分两类：
        # - 提示词 / 工具集 / yolo / max_turns 取 **live 有效值**：agent 由
        #   AgentTemplate.from_agent 按 live 构造，记录必须与之一致，否则子会话
        #   live 与 resume 分叉。append_system_prompt 先按 live 值写入（子会话
        #   构造时继承，也兼容 hook 注入只存在于内存的存量会话），随后
        #   before_session_start 在子会话上生效、sync 把注入后的结果覆盖落盘。
        # - thinking / reasoning_effort 取**显式记录**（可能为 None）：派生默认值
        #   （provider 协议默认）固化进记录会让子会话请求体带上源会话没有的显式
        #   配置（前缀身份被破坏），跨协议切模型时更会把一种协议的默认值贴到另一种
        #   协议上。
        store.save_metadata(
            new_session_id,
            SessionMetadata(
                workspace=source.session_workspace,
                forked_from=session_id,
                template_name=source.template_name,
                model_name=source.agent.model,
                provider_name=source.agent.model_provider.name,
                system_prompt=source.context_manager.setin_system_prompt or None,
                append_system_prompt=(
                    source.context_manager.append_system_prompt or None
                ),
                tools=tool_refs(source.agent.tools),
                thinking=source.persisted_thinking,
                reasoning_effort=source.persisted_reasoning_effort,
                yolo=source.agent.yolo,
                max_turns=source.agent.max_turns,
                last_interaction=datetime.now().isoformat(),
            ),
        )

        template = AgentTemplate.from_agent(source.agent, name=source.template_name)
        new_session = Session.from_template(
            template=template,
            session_id=new_session_id,
            messages=new_messages,
            store=store,
            workspace=source.session_workspace,
        )
        # 先注册 / 刷新计时器再跑 hook（与 create_session 同序）：hook 内 emit 的
        # 事件能命中 SessionReaper 的 touch 订阅，hook 内查 SM 也能看到子会话。
        self._sessions[new_session_id] = new_session
        self.touch(new_session_id)

        # fork 也是"创建新 session"（session id 变化）：before_session_start 在子会话
        # 上生效，hook 注入的环境信息属于这个新 session。子会话已继承源的追加内容
        # （上面的 metadata 快照 + 构造时还原），不自幂等的 hook 会在其上再叠一层
        # （钩子自身的问题，钩子系统重做时收口，见 docs/zh/custom-tools.md 的 hook
        # 契约与 issue #131）；session id 变化本就让上游缓存无法复用
        # （见 docs/dev/architecture.md「压缩与缓存哲学」）。
        hooks.invoke("before_session_start", new_session)

        # 记录对齐（hook 注入后的 append + **实际生效**的工具集：按 ref 还原可能
        # 降级——远程宿主断连——记录必须与 live 一致，否则重启后声明集凭空变化）。
        new_session.sync_append_system_prompt()
        new_session.sync_tools_record()

        return new_session, draft

    # ── 标签 ─────────────────────────────────

    def set_session_tags(
        self,
        session_id: str,
        *,
        add: Iterable[str] = (),
        remove: Iterable[str] = (),
    ) -> TagMutation:
        """按 session id 原子增删标签（幂等；读或写都不触发水合）。

        标签是**持久 metadata**（不是运行时状态），因此有两条互斥路径：

        - 已在内存 → 经 ``Session.apply_tag_ops`` 改内存元数据并落盘
          （内存对象是磁盘事实的同一来源，绕开它会被后续 save 回写覆盖）；
        - 未加载 / 已逐出 → 直接 store 读改写，**不水合**——给旧会话打
          favorite 不会把它"弄醒"变成 idle（会话保持 inactive，内存零代价）。

        打标时间（``tag_meta``）随同一处变更维护：新增记时间、移除删记录，
        与 tags 一次落盘。

        session id 先过格式闸门（``_resolve_with_store``）：不合规的值按
        "不存在"处理，绝不触达 store（防路径穿越）。

        add / remove 皆空 = 纯读（返回当前标签，不产生任何写）。

        Raises:
            LookupError: 会话不存在（内存与磁盘都没有；id 格式不合规同价）
            ValueError: 标签非法 / 超过单会话上限
        """
        resolved = self._resolve_with_store(session_id)
        if resolved is None:
            raise LookupError(f"Session not found: {session_id}")
        resolved_id, store = resolved

        session = self._sessions.get(resolved_id)
        if session is not None:
            return session.apply_tag_ops(add=add, remove=remove)

        metadata = store.load_metadata(resolved_id) or SessionMetadata()
        mutation = apply_tag_ops(
            metadata.tags, add=add, remove=remove, meta=metadata.tag_meta
        )
        if mutation.added or mutation.removed:
            metadata.tags = mutation.tags or None
            metadata.tag_meta = mutation.tag_meta or None
            store.save_metadata(resolved_id, metadata)
        return mutation

    # ============================================================
    # 外部方法：逐出（eviction）
    # ============================================================

    def touch(self, session_id: str) -> None:
        """刷新会话的空闲计时器（任何状态变化都算一次「在场」）。

        由 SessionReaper 的 EventBus 订阅驱动——事件流即会话状态变化的
        全量来源，无需在各调用点插桩。未加载的 session id 静默忽略。
        """
        if session_id in self._sessions:
            self._last_active[session_id] = time.monotonic()

    def ensure_loaded(self, session_id: str) -> Session:
        """取会话；不在内存时从磁盘水合（复用 resume 路径）。

        逐出后的入口都应经此取会话：被逐出不再是「会话不存在」，
        只是「不在内存」——磁盘上也没有才 LookupError。

        Raises:
            LookupError: 内存与磁盘都没有该会话
        """
        session = self._sessions.get(session_id)
        if session is not None:
            return session
        return self.resume_session(session_id)

    def evict(self, session_id: str, reason: str) -> bool:
        """摘除会话并异步拆解（幂等；未加载返回 False）。

        摘除（pop）是同步的、且发生在拆解协程真正运行之前——单线程事件
        循环下即原子，拆解失败不影响「已逐出」这个事实。拆解任务登记在
        `_teardowns` 里（强引用防 GC，测试可等待其完成）。
        """
        session = self._sessions.get(session_id)
        if session is None:
            return False
        # 先建任务：无运行中的事件循环时在此抛错，状态未被改动（会话保持
        # 在场），不会留下"已摘除却没拆解"的孤儿对象。
        task = asyncio.create_task(self._teardown(session, reason))
        self._sessions.pop(session_id, None)
        self._last_active.pop(session_id, None)
        self._teardowns.add(task)
        task.add_done_callback(self._teardowns.discard)
        return True

    async def _teardown(self, session: Session, reason: str) -> None:
        """拆解会话运行期资源并记一行日志（异常不逃逸）。"""
        try:
            await session.aclose()
        except Exception as e:
            log.error(f"Session teardown failed ({session.session_id}): {e}")
        log.info(f"Session evicted: {session.session_id} ({reason})")

    def evict_idle_sessions(
        self, ttl_seconds: float, now: float | None = None
    ) -> list[str]:
        """逐出所有「空闲超过 ttl 且未被钉住」的会话，返回 id 列表。

        同步完成「决策 + 摘除」；拆解异步收尾。`now` 可注入（单测用假时钟）。
        """
        current = time.monotonic() if now is None else now
        evicted: list[str] = []
        for session_id, session in list(self._sessions.items()):
            if self._blocked_reason(session) is not None:
                continue
            last_active = self._last_active.get(session_id, current)
            idle = current - last_active
            if idle < ttl_seconds:
                continue
            if self.evict(session_id, reason=f"idle {int(idle)}s"):
                evicted.append(session_id)
        return evicted

    def release_session(self, session_id: str) -> tuple[bool, str]:
        """显式逐出（用户主动 release）：忽略空闲时长，不忽略钉住条件。

        Returns:
            (released, detail)：released=False 表示会话本就不在内存（幂等，
            不是错误——它已经在「逐出」这个目标状态里了）。

        Raises:
            LookupError: 内存与磁盘都没有该会话
            RuntimeError: 被钉住（忙碌 / 有待处理输入 / 被订阅 / 非持久后端）
        """
        session = self._sessions.get(session_id)
        if session is None:
            if self._resolve_with_store(session_id) is None:
                raise LookupError(f"Session not found: {session_id}")
            return False, "not loaded"
        blocker = self._blocked_reason(session)
        if blocker is not None:
            raise RuntimeError(f"session cannot be released: {blocker}")
        self.evict(session_id, reason="released")
        return True, "released"

    async def wait_teardowns(self) -> None:
        """等待全部在途拆解完成（测试与收尾路径用）。"""
        while self._teardowns:
            await asyncio.gather(*list(self._teardowns), return_exceptions=True)

    def _blocked_reason(self, session: Session) -> str | None:
        """不可逐出的原因；None = 可以逐出。

        四类：在飞 turn（working / waiting）、inbox 有待处理输入、非持久
        后端、有订阅的客户端。前两者是正确性（拆解会打断它们），后两者是
        语义（memory 后端逐出即毁数据；订阅中的会话是用户的工作集）。
        """
        if session.status != "idle":
            return f"status={session.status}"
        if session.agent.has_pending_input:
            # 消息已入队但 turn 未开始（status 仍 idle）——直接投递
            # `agent.post()` 的路径（工具侧内部投递等）不经过
            # SM._post，不会 touch 计时器，只靠 timer 会漏判。
            return "pending input"
        if not session.store.durable:
            return f"non-durable store '{session.store.name}'"
        if event_bus.subscribers_of(session.session_id):
            return "subscribed"
        return None

    # ============================================================
    # 外部方法：查询
    # ============================================================

    def list_sessions(self) -> list[SessionInfo]:
        """列出所有有效 session（跨 stores 聚合），按「活跃优先 + 最后交互时间降序」。

        每个 session 携带运行时 `status`：
        - 已加载进内存（在 `self._sessions` 中）→ 取 live 状态（idle/working/waiting）
        - 未 resume → `inactive`

        排序口径（唯一事实来源，前端按原序渲染、不再重排）：

        1. `status != "inactive"` 的（= 已在内存里的工作集）在前，未加载的在后；
        2. 组内按 `_timestamp_key`（最后一次交互时间，见该函数的归一化回退）降序；
        3. 完全并列（含都取不到时间）时按 session id 升序——**只为定序**，不是
           优先级：没有它，并列项的先后就取决于 store 的枚举顺序（`iterdir()` /
           SQL 返回序），同一个列表两次请求可能给出不同顺序。

        `status` 只用来区分 active / inactive，不再有组内优先级：`waiting`
        （正在等用户回答）不因为状态本身提前——旧的「waiting > working > idle」
        排序键已从前端删除，需要突出 waiting 时看面板上的状态图标（`?`）。

        workspace 不参与排序：workspace 匹配曾作为前端的第一排序键，让
        「在哪启动 TUI」压过了「正在用哪几个会话」——本方法不复制该语义。
        """
        result = []
        for store in self._stores.values():
            for summary in store.list_summaries():
                metadata = summary.metadata
                name = metadata.session_name or summary.first_user_message
                # 无名且无标的照旧隐藏——"带标"按**清洗后**的集合判定（仅含
                # 非法标签的脏会话不当作带标，避免空标题噪音；store 层的
                # 原始字段判定只负责把候选交给这里）。
                if not name and not sanitize_tags(metadata.tags):
                    continue

                loaded = self._sessions.get(summary.id)
                tags = sanitize_tags(metadata.tags)
                result.append(
                    SessionInfo(
                        id=summary.id,
                        name=name,
                        workspace=metadata.workspace,
                        last_interaction=metadata.last_interaction,
                        status=loaded.status if loaded is not None else "inactive",
                        tags=tags,
                        tag_meta=sanitize_tag_meta(metadata.tag_meta, tags),
                    )
                )

        # 活跃优先（False < True），组内时间新的在前；并列时按 id 定序（见 docstring）。
        result.sort(key=lambda s: (s.status == "inactive", -_timestamp_key(s), s.id))

        return result

    # ============================================================
    # 外部方法：消息路由入口
    # ============================================================

    async def _post(
        self,
        content: str,
        request_id: str | None = None,
        session_id: str | None = None,
        tool_call_id: str | None = None,
    ) -> None:
        """路由消息到指定 session。（内部方法，由 WingRuntime 调用）

        Contextvars 由 WingRuntime.post() 统一管理，此方法不设置/恢复。

        - 不在内存的会话先按需水合（逐出后仍可投递）
        - / 开头且匹配 prompt 命令 → 展开为纯文本后投递
        - 否则 → 直接投递给 session.post()

        Raises:
            LookupError: 内存与磁盘都没有该会话（调用方负责映射错误面）
        """
        assert session_id is not None, "session_id is required by WingRuntime"

        session = self.ensure_loaded(session_id)

        event_bus.emit(
            DeliveredEvent(
                session_id=session.session_id,
                target=EventTarget(scope="session"),
            )
        )

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
