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
from contextlib import ExitStack
import time
from collections.abc import Collection, Iterable, Sequence
from uuid import uuid4
from datetime import datetime
from typing import TYPE_CHECKING

from wing.chain import TrackedList
from wing.common.logger import log, log_excerpt
from wing.common.utils import (
    InvalidInputError,
    generate_session_id,
    is_valid_session_id,
    require_utf8,
    validate_session_id,
)
from wing.config import get_config
from wing.hooks import hooks
from wing.media import file_too_large_message, upload_image_ref
from wing.event import (
    DeliveredEvent,
    EventTarget,
    SessionInfo,
)
from wing.event_bus import event_bus
from wing.commands import expand_prompt_command
from wing.request_context import session_context
from wing.schema import ChainNode, MediaRef, Message
from wing.store import (
    ArchivedSession,
    FileRef,
    SessionMetadata,
    SessionStore,
)
from wing.store.base import canonical_record, validate_file_name

from .model_binding import resolve_model_binding
from .override import AgentOverride, validate_override_utf8
from .session import Session, tool_refs, validate_media_refs, validate_tool_refs
from .tags import PIN_TAG, TagMutation, apply_tag_ops, sanitize_tag_meta, sanitize_tags
from .template import AgentTemplate, AgentTemplateManager

if TYPE_CHECKING:
    from .override import AgentOverride


class SessionArchivedError(Exception):
    """目标会话已被归档（live 区找不到、归档区命中）。

    **刻意不是 ``LookupError`` 的子类**：网关路由把 LookupError 统一折成 404 +
    "session not found"，而"已归档"是一条**可行动**的存在性结论（"恢复它再继续"）
    ——gateway 为它单独注册异常处理器，回 404 + 结构化 ``error="session_archived"``，
    与"真不存在"（``not_found``）区分。子类化会让所有既有 ``except LookupError``
    把它吞成普通 404，提示就没了。

    message 面向客户端（HTTP detail / WS error 帧 / TUI toast）：只说"要恢复"，
    不背 CLI 语法（命令怎么拼是各自前端的事；CLI 自己组装提示）。
    """

    def __init__(self, session_id: str) -> None:
        super().__init__(f"session {session_id!r} is archived; restore it before use")
        self.session_id = session_id


def _fork_slice(
    records: Iterable[dict],
    target_uuid: str,
) -> tuple[list[dict], str | None]:
    """切出 fork 的记录前缀（**append 顺序**）与 draft。

    - ``"current"`` → 全部记录，draft 为空串；
    - 其他 → 目标记录**之前**的全部记录（目标自身不进拷贝：它由响应里的
      draft 重新发送），draft = 目标记录的 content。

    draft 走 ``Message`` 的访问器（assistant 的 content 由 ``content_blocks``
    派生——记录里**不存**副本），因此与"用户看到的那条消息"同源：目标可以是
    压缩节点（``[Compact] …``）或任何带文本的 assistant 记录。

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
            return prefix, _record_draft(record)
        prefix.append(record)
    raise ValueError(f"uuid {target_uuid!r} not found in {len(prefix)} record(s)")


def _record_draft(record: dict) -> str:
    """目标记录的 draft 文本：走 ``Message`` 派生（assistant 的 content 在
    ``content_blocks`` 里，记录本身不带副本）；脏记录按"无文本"降级。

    fork 是**读请求**，不能因为一条记录校验不过就整单失败——降级为空串与
    "目标没有可发送文本"同义。``content`` 不做类型校验（私有属性直通），
    因此非字符串值也在这里折成空串。
    """
    try:
        draft = Message.model_validate(record).content
    except Exception:
        return ""
    return draft if isinstance(draft, str) else ""


def _warn_alias(requested: str, resolved: str) -> None:
    """别名（请求 id ≠ 存储键）时说清发生了什么——**不做静默归一**。

    大小写 / Unicode 归一化不敏感的文件系统上（macOS APFS 默认 / Windows NTFS），
    ``team-a`` 与 ``Team-A`` 是同一份日志：只会有一条 warning，然后按磁盘真名
    继续（内存索引也用真名，两个拼写因此落在同一个会话上）。
    """
    if requested == resolved:
        return
    log.warning(
        f"session id {requested!r} resolves to existing session {resolved!r} on this "
        "filesystem (case / Unicode-normalization insensitive); using the on-disk id"
    )


def _session_epoch(session_id: str, last_interaction: str | None) -> float | None:
    """归一化会话的「最后一次交互时间」为可比较的 epoch 秒；未知返回 None。

    「有啥用啥」的三级回退：``metadata.last_interaction``（ISO 字符串或数值）→
    缺失 / 不可解析时用 session id 前缀（``YYYYMMDD-HHMMSS``）→ 都没有返回 None。

    时区口径：naive 的 ISO 字符串（仓库内唯一实际写法，`datetime.now().isoformat()`）
    与 id 前缀回退按**宿主本地时区**折算 epoch，带 `Z` / `+00:00` 的 aware 字符串按
    UTC 折算——同一份列表里不要混写两种字符串，否则按本地 UTC 偏移错序（小时级）。
    数值分支是防御代码（`SessionMetadata.last_interaction` 只声明 `str`）。

    调用方分两类：**排序**（``list_sessions``）把 None 折成 0.0 用（"排最后"，
    永不抛——排序键不可解析不该让整个列表 500）；**归档判定**必须把 None 当
    「年龄未知」跳过（不发明时间，保守不动未知会话）。
    """
    ts = last_interaction
    if ts is not None:
        if isinstance(ts, str):
            try:
                dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
                return dt.timestamp()
            except Exception:
                pass
        elif isinstance(ts, (int, float)):
            return float(ts)
    prefix = session_id[:15] if len(session_id) >= 15 else session_id
    try:
        dt = datetime.strptime(prefix, "%Y%m%d-%H%M%S")
        return dt.timestamp()
    except Exception:
        return None


def _remap_record_uuids(records: list[dict]) -> list[dict]:
    """复制记录（顺带规范化）并把链拓扑 uuid 全量重映射到子会话的 uuid 空间。

    重映射 ``uuid`` / ``parent_uuid`` / ``unzip_last_uuid`` 三个键。前缀口径下
    所有引用都指向前缀内部（parent 必然更早创建、unzip 指向区间末），因此
    重映射后子记录集自洽：不留悬空引用，也不需要清空任何引用——压缩节点的
    unzip 指向重映射后的区间末记录，子会话的 fork 候选列表与源会话一致。

    缺省是防御：万一出现前缀外的引用，宁可让它成为根节点（删键 = 缺席即默认），
    也不留跨 session 的引用。

    ``canonical_record`` 让**复制也走落盘规范形**：fork 直接复用源记录的原始
    dict（不经 ``TrackedList._to_record``），不过这一道，存量旧形态的冗余字段
    （派生扁平字段 / null）就会被逐字节搬进子会话，越 fork 越多。

    只做**顶层浅拷贝**：重写的三个键都在顶层，而记录（含嵌套值）对读取方
    一律只读（``MessageLog`` 的记录契约：file 后端每次重新解析，memory
    后端直接交出内部结构，两边都没有就地改写记录的路径），深拷贝没有额外
    保护面。
    """
    clones = [canonical_record(record) for record in records]
    uuid_map: dict[str, str] = {}
    for clone in clones:
        uuid = clone.get("uuid")
        if isinstance(uuid, str):
            uuid_map[uuid] = str(uuid4())
    for clone in clones:
        for key in ("uuid", "parent_uuid", "unzip_last_uuid"):
            value = clone.get(key)
            if not isinstance(value, str):
                continue
            mapped = uuid_map.get(value)
            if mapped is None:
                # 前缀外引用：**删键**而不是写 None（缺席即默认，与 None 对
                # 加载等价——记录重新成为根节点，且不把 null 带进子会话）。
                clone.pop(key, None)
            else:
                clone[key] = mapped
    return clones


def _message_media_ids(nodes: Iterable[ChainNode]) -> list[str]:
    """链节点引用的媒体 id（去重、保持出现序）。

    ``ChainNode`` 家族里只有 ``Message`` 带 ``media``（事件没有）——按属性
    探测，加载路径对两类记录混排的链无感。
    """
    seen: dict[str, None] = {}
    for node in nodes:
        for ref in getattr(node, "media", None) or []:
            seen.setdefault(ref.id, None)
    return list(seen)


def _record_media_ids(records: Iterable[dict]) -> list[str]:
    """raw 记录引用的媒体 id（去重、保持出现序；坏形态跳过）。

    记录契约归 ``MessageLog`` 管：这里只做防御性取值（media 不是数组、
    元素不是 dict、id 不是 str 一律跳过）——fork 复制是 best-effort 路径，
    不值得为一条坏记录炸掉整个 fork。
    """
    seen: dict[str, None] = {}
    for record in records:
        refs = record.get("media")
        if not isinstance(refs, list):
            continue
        for ref in refs:
            if isinstance(ref, dict) and isinstance(ref.get("id"), str):
                seen.setdefault(ref["id"], None)
    return list(seen)


def _materialize_loaded_media(
    store: SessionStore, session_id: str, nodes: Iterable[ChainNode]
) -> None:
    """加载路径：把活跃链引用的媒体对象幂等物化进会话目录（best-effort）。

    老会话的对象在遗留共享池（``<root>/.media/``）里，读路径虽能双读兜底，
    但"会话自包含"要求对象在**首次加载**时就搬进 ``<sid>/media/``——此后
    整个会话目录拷贝到另一台机器即可用。对象通常已在目录里（快路径 stat
    即返回），miss 时才碰老池。失败只 WARN（含历史里的非法 id）：读取侧
    双读仍在，最坏是该位降级为占位文本，绝不阻断加载。
    """
    for media_id in _message_media_ids(nodes):
        try:
            store.materialize(session_id, media_id)
        except Exception as e:  # noqa: BLE001 —— 迁移失败不能阻断会话加载
            log.warning(
                f"Session {session_id}: materialize media '{media_id[:8]}' "
                f"failed: {e} (read path keeps its fallback)"
            )


def _copy_session_media(
    store: SessionStore, source_id: str, child_id: str, records: Iterable[dict]
) -> None:
    """fork：把子会话记录引用的媒体对象复制进子会话目录（自包含；best-effort）。

    复制源是**源会话目录优先、老池回退**（经 ``read_media``，顺带把老池命中
    物化进源目录）。失败只 WARN 不阻断 fork——读取侧双读兜底仍在，代价是复制
    失败的那个对象在子会话里会降级为占位（回退池不含源会话目录，这是刻意的：
    不给"自包含"留第二条隐式依赖）。判据是常规路径全部复制成功（probe 钉住）。
    """
    for media_id in _record_media_ids(records):
        try:
            data = store.read_media(source_id, media_id)
            if data is None:
                log.warning(
                    f"fork: media '{media_id[:8]}' referenced by session "
                    f"'{source_id}' not found (session dir or legacy pool); "
                    f"child '{child_id}' will degrade it to a placeholder"
                )
                continue
            store.write_media(child_id, media_id, data)
        except Exception as e:  # noqa: BLE001 —— 复制失败不能阻断 fork
            log.warning(
                f"fork: copying media '{media_id[:8]}' from '{source_id}' to "
                f"'{child_id}' failed: {e}"
            )


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
        self._template_manager = AgentTemplateManager(config.agents, config)

    @property
    def template_manager(self) -> AgentTemplateManager:
        """Agent 模板管理器。"""
        return self._template_manager

    def reload_templates(self) -> None:
        """按**当前 config** 重建模板管理器（config 热重载后调用）。

        模板是 config 的派生状态（`agents[].model` 经 id 表解析成调用名 +
        provider）：不重建的话，「reload 后新建的会话用旧模板（旧 id 映射）、
        已存在会话 resume 时用新映射」两条路径会分叉。重建只影响**未来的**
        模板解析；已在内存的会话保持自己的 agent（切模型是用户的显式动作）。

        Raises:
            ValueError: 新 config 的 `agents[]` 无法解析（理论上不可达——
                config 加载期已强制；调用方按「config 项的一部分」处理）。
        """
        config = get_config()
        self._template_manager = AgentTemplateManager(config.agents, config)

    def _template_or_default(self, name: str | None) -> AgentTemplate:
        """按记录里的模板名取模板；缺失 / 已不存在（config 演化）→ 默认模板。

        「metadata 是模板的唯一来源」这条规则只有一份实现，resume（真的按模板
        重建 agent）与列表投影（未加载会话的模型列）共用它——否则两边会给出
        不同的默认模板答案。
        """
        template = self._template_manager.get(name) if name is not None else None
        return template if template is not None else self._template_manager.default

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
        parent_session_id: str | None = None,
    ) -> Session:
        """创建（或按 id 收养）session。

        ``session_id`` 是 **create-or-adopt** 入口（编排方自带 id 的场景，如 Claude Agent SDK 系
        消费方用自己的 UUID 建会话）：给定时该 id 即最终 session id——已存在（内存或任 store）则
        **收养**既有会话，语义等同 :meth:`resume_session`（模板 / workspace 来自 metadata，agent
        覆盖只应用 resume 子集，见 :meth:`Session.apply_resume_override`），且不触发
        `before_session_start`（session id 未变，是"恢复"而非"创建新会话"）；不存在则以该 id 建
        会话（此时 agent 覆盖是创建语义：全字段应用）。不给 ``session_id`` 时 id 由后端生成（见
        :meth:`_generate_session_id`）——**既有策略是默认，不是唯一**。

        ``parent_session_id`` 是**血缘**（谁创建了我，产品语义见 docs/dev/session-lineage.md）：
        只做格式卫生（同 session id 规则）、**不校验存在性**（弱引用——幽灵父由展示层降级为根），
        与 ``session_id`` 同时给出且字面相等（自指）即 400。带父创建时在本方法内一次写入
        metadata（此后没有修改路径）；**收养路径忽略它**（resume 语义：既有会话的记录不被改写）
        并记 warning。无父创建零字段、零写盘。

        Args:
            template_name: Agent 模板名称，None 时使用默认模板（仅新会话生效）
            workspace: 工作目录（仅新会话生效）
            agent_override: AgentOverride 参数覆盖（None 字段不覆盖 template 值；收养路径只应用
                model_id/effort/tools）
            backend: 存储后端名称（如 file/memory），None 时使用默认后端
            tags: 创建即打标 / 收养时并入（经 :meth:`set_session_tags` 同一套校验）
            session_id: 指定 session id（create-or-adopt）；None = 自生成
            parent_session_id: 创建者会话 id（血缘）；None = 根会话（用户直接创建）

        Raises:
            ValueError: session_id / parent_session_id 不合规（含自指）/ 模板不存在 /
                backend 未知 / 标签非法
        """
        # 闸门先于一切：不合规的 id 不得触达任何 store（防路径穿越），也必须
        # 在任何写盘之前失败——否则一个失败的 create 会在磁盘上留下半份
        # metadata，下一次同 id 的 create 就会"收养"一个幽灵会话。
        # parent 同属纯校验（血缘是创建动作的事实，脏值即请求错），且**先于收养
        # 分支**：自指在任何路径上都是 400，不被"已存在 → 收养"吞掉。
        if session_id is not None:
            validate_session_id(session_id)
        if parent_session_id is not None:
            validate_session_id(parent_session_id, field="parent session id")
            if session_id is not None and parent_session_id == session_id:
                raise ValueError(
                    "parent session id must differ from session id "
                    f"(self-reference: {session_id!r})"
                )
        if session_id is not None:
            adopted = self._adopt_session(
                session_id, agent_override=agent_override, tags=tags
            )
            if adopted is not None:
                if parent_session_id is not None:
                    log.warning(
                        f"Session {adopted.session_id}: create-or-adopt ignores "
                        f"parent session id {parent_session_id!r} "
                        "(lineage is written once, at creation)"
                    )
                return adopted

        backend_name = backend if backend is not None else self._default_backend
        store = self._stores.get(backend_name)
        if store is None:
            raise ValueError(
                f"Unknown storage backend '{backend_name}'. "
                f"Available: {list(self._stores)}"
            )

        # 标签、覆盖文本与工具 ref 的纯校验提到最前（都不写盘）：任何非法输入在
        # 任何副作用之前 raise，"失败即零残留"对 create-or-adopt 尤其重要（重试
        # 必须还是干净状态）。全部必须在**认领键**之前——认领会建会话目录。
        # model_id 的查表同理前置：未命中在 `apply_agent_override`（认领之后）才
        # 会 raise，留下一个失败产生的空会话目录。
        if tags:
            apply_tag_ops([], add=tags)
        if agent_override is not None:
            validate_override_utf8(agent_override)
            if agent_override.tools is not None:
                validate_tool_refs(agent_override.tools)
            if agent_override.model_id is not None:
                get_config().require_model(agent_override.model_id)
        if workspace is not None:
            require_utf8(workspace, field="workspace")

        if template_name is not None:
            template = self._template_manager.get(template_name)
            if template is None:
                raise ValueError(
                    f"Agent 模板 '{template_name}' 不存在。"
                    f"可用模板: {self._template_manager.all_names}"
                )
        else:
            template = self._template_manager.default

        # 指定 id 时它就是最终 id；否则自生成（默认策略）。
        #
        # **认领键**（create 路径专用，会建目录）：大小写 / 归一化不敏感的文件系统
        # 上，"两个只差大小写的 id"在两个会话都还没落盘时看不见别名——`mkdir` 的
        # 成败把它变成确定事实（EEXIST ⇒ 该路径已解析到某个目录，取真实目录名）。
        # 认领回来的键若已在内存（空会话变体），走收养而不是新建：内存键 == 目录名
        # 这条不变量对所有写入路径成立，同一目录才不会被两个 Session 各写一份链。
        sid = (
            self._generate_session_id()
            if session_id is None
            else store.claim_session_id(session_id)
        )
        if session_id is not None:
            _warn_alias(session_id, sid)
            if sid in self._sessions:
                if parent_session_id is not None:
                    log.warning(
                        f"Session {sid}: create-or-adopt ignores parent session "
                        f"id {parent_session_id!r} (lineage is written once, "
                        "at creation)"
                    )
                return self._finish_adoption(
                    sid, agent_override=agent_override, tags=tags
                )

        messages: TrackedList[ChainNode] = TrackedList(store.open_log(sid))

        session = Session.from_template(
            template=template,
            session_id=sid,
            messages=messages,
            store=store,
            workspace=workspace,
        )

        # 血缘落盘：创建时刻的一次性事实（此后没有修改路径），写在**构造成功
        # 之后**——构造期失败（provider 解析等）因此不会留下"有 parent 无会话"的
        # 记录（那种残骸会让同 id 的下一次 create 收养到一个幽灵事实）。此后任何
        # 一次 _save_metadata（hook 注入 / 打标 / 用量落盘）写的都是这个已带上字段
        # 的内存对象，不会被抹掉；无 parent 时不写（零字段、零写盘，存量不变）。
        if parent_session_id is not None:
            session.record_parent_session_id(parent_session_id)

        # agent override 需在 session 完全构建后应用
        if agent_override is not None:
            session.apply_agent_override(agent_override)

        # 创建即打标（原子语义：校验在写盘前完成，非法标签此刻 raise——
        # 会话尚未注册、磁盘零痕迹）
        if tags:
            session.apply_tag_ops(add=tags)

        self._register(session)

        # 触发 before_session_start hook，随后把 hook 注入落盘：resume 重建 CM
        # 时恢复同一系统提示词（前缀身份不因换入内存而漂移）。
        hooks.invoke("before_session_start", session)
        session.sync_append_system_prompt()

        log.info(f"Session created: {sid} (template={template.name})")
        return session

    def _register(self, session: Session) -> None:
        """把会话放进工作集并激活用量账本（**唯一注册入口**）。

        注册即激活（`Session.activate_usage`）：账本的事件订阅跟随在场生命
        周期（注册 → 逐出退订），构造期不订阅——构造与注册之间可能因覆盖 /
        标签校验失败而中止，那类被放弃的会话必须能被 GC（构造期订阅会把它
        永久钉在全局总线上，而它持有整份消息历史）。
        """
        session_id = session.session_id
        self._sessions[session_id] = session
        session.activate_usage()
        self.touch(session_id)

    # ── resolve ───────────────────────────────

    def _resolve_with_store(self, session_id: str) -> tuple[str, SessionStore] | None:
        """跨 stores 精确解析 session id，返回 (真实键, store)。

        优先命中内存中的 session，再按 stores 注册顺序查后端是否存在。

        **闸门在最前**（`is_valid_session_id`）：不合规的值一律按"不存在"处理，
        绝不允许它进入任何 store 调用——file 后端拿它拼路径，这是路径穿越的
        唯一入口（gate 在这里，所有网络路径都经过本方法）。

        **返回的是存储里的真实键**（不是请求字符串）：大小写 / Unicode 归一化不
        敏感的文件系统上（macOS APFS / Windows NTFS），``team-a`` 与 ``Team-A``
        解析到同一个目录——按请求字符串建内存索引会让两个 Session 共用一份
        history.jsonl（静默数据混合）。归一是**正确性要求**：调用方拿到的一律是
        磁盘上的真名（别名会留一行 warning），因此「两个不同 id 指向同一目录」
        在内存里不可能出现。

        解析失败与闸门拒绝最终都映射为 404，不向客户端区分（不给探测反馈）。
        """
        if not is_valid_session_id(session_id):
            return None
        if session_id in self._sessions:
            return session_id, self._sessions[session_id].store
        for store in self._stores.values():
            canonical = store.resolve_stored_id(session_id)
            if canonical is None or not store.exists(canonical):
                # 目录 / 键存在但不是会话（空壳、杂物目录）与"不存在"同价。
                continue
            _warn_alias(session_id, canonical)
            return canonical, store
        return None

    def read_session_aux(self, session_id: str, key: str) -> dict | None:
        """按 id 读取会话的 aux kv（**纯解析，不水合**）；不存在 / 不可读 → None。

        读的是「与消息日志同生命周期的边车数据」（如缓存账本）：查询类端点不该为了
        读一份账本把会话换入内存。解析走 :meth:`_resolve_with_store`（同一套闸门与
        别名归一），因此读写键与账本落点必然一致。
        """
        resolved = self._resolve_with_store(session_id)
        if resolved is None:
            return None
        canonical, store = resolved
        try:
            return store.open_log(canonical).read_aux(key)
        except Exception as e:
            log.warning(f"Failed to read aux '{key}' for session '{canonical}': {e}")
            return None

    def _archived_store(self, session_id: str) -> SessionStore | None:
        """该 id 落在（任一后端的）归档区时返回其 store；否则 None。

        归档区**不参与** :meth:`_resolve_with_store` 的正常解析（冻结语义）：
        这里的探测只服务"找不到时给一句可行动的提示"。
        """
        if not is_valid_session_id(session_id):
            return None
        for store in self._stores.values():
            if store.resolve_archived_id(session_id) is not None:
                return store
        return None

    def is_archived(self, session_id: str) -> bool:
        """该 id 是否在（任一后端的）归档区（纯查询，不涉 live 区）。

        回执用的**真实状态**：显式归档一个空会话是 no-op（它没进归档区），
        响应里的 ``archived`` 不能照抄请求意图。
        """
        store = self._archived_store(session_id)
        return store is not None and store.is_archived(session_id)

    def missing_session_error(self, session_id: str) -> Exception:
        """构造「会话不存在」的错误：归档区命中 → :class:`SessionArchivedError`，
        否则普通 ``LookupError``。

        只在**找不到**的路径上探测归档区（每后端一次目录 stat）——正常路径零
        额外成本。id 不合规与真不存在同价（LookupError），不向客户端区分。
        """
        if self._archived_store(session_id) is not None:
            return SessionArchivedError(session_id)
        return LookupError(f"Session not found: {session_id!r}")

    def resume_session(
        self,
        session_id: str,
        agent_override: "AgentOverride | None" = None,
    ) -> Session:
        """恢复已有 session（精确匹配 session id）。已在内存中则直接返回。

        模板只从 metadata.template_name 解析——resume 不接受显式模板：**metadata 是模板的唯一
        来源**，要换模板请在恢复后走 `session/update`（agent 字段）。template_name 缺失或已不存在
        于 config 时回退默认模板。

        ``agent_override`` 是 resume 语义的参数覆盖（编排方 `--model` 等）：只应用 `model_id` /
        `effort` / `tools` 子集——见 :meth:`Session.apply_resume_override`（不改链上前缀是不变量）。

        Args:
            session_id: 目标 session ID（须为完整 ID）
            agent_override: 恢复后应用的参数覆盖（None = 不覆盖）

        Returns:
            恢复后的 Session 实例

        Raises:
            LookupError: session 不存在
            ValueError: 覆盖里的 model_id 未命中 id 空间 / 工具引用无法解析
        """
        result = self._resolve_with_store(session_id)
        if result is None:
            raise self.missing_session_error(session_id)
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
        tpl = self._template_or_default(
            metadata.template_name if metadata is not None else None
        )

        messages: TrackedList[ChainNode] = TrackedList.load(
            store.open_log(resolved), Message
        )
        # 会话自包含：把活跃链引用的媒体对象幂等物化进会话目录（老池兼容面；
        # 见 _materialize_loaded_media）。失败 WARN 不阻断加载。
        _materialize_loaded_media(store, resolved, messages)
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
        self._register(session)
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
        - `agent_override` 走 resume 子集（model_id/effort/tools）；
        - `tags` 按 add 语义并入（幂等；非法标签在此 ValueError，零写盘）；
        - **不触发 `before_session_start`**：session id 未变，这是"恢复既有
          会话"而非"创建新会话"（该 hook 的语义边界就是"新 session id"）。

        Returns:
            收养到的 Session；``session_id`` 不存在（或未成会话）时返回 None。

        Raises:
            SessionArchivedError: 该 id 落在归档区——**不做隐式恢复**（冻结语义
                没有例外路径；要接着用先显式 unarchive），也绝不因此新建一个
                同 id 的 live 会话（两个区同 id 是数据分裂）。
        """
        if self._resolve_with_store(session_id) is None:
            if self._archived_store(session_id) is not None:
                raise SessionArchivedError(session_id)
            return None
        return self._finish_adoption(
            session_id, agent_override=agent_override, tags=tags
        )

    def _finish_adoption(
        self,
        session_id: str,
        *,
        agent_override: "AgentOverride | None",
        tags: Iterable[str] | None,
    ) -> Session:
        """对**已知存在**的会话应用收养语义（resume + 覆盖子集 + tags）。

        与 :meth:`_adopt_session` 分开，是因为"键已认领但还没成会话"的空会话变体
        （见 create_session 的认领注释）也要走这条路——那时 ``_resolve_with_store``
        查不到它（空会话不是会话），但它确实已经在内存里了。
        """
        # 标签先纯校验：非法标签不得留下"覆盖已应用"的半截状态。
        if tags:
            apply_tag_ops([], add=tags)
        session = self.resume_session(session_id, agent_override=agent_override)
        if tags:
            session.apply_tag_ops(add=tags)
        log.info(f"Session adopted: {session.session_id} (create-or-adopt)")
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
        child_records = _remap_record_uuids(copied)
        child_log.append(child_records)
        # 子会话自包含：把记录（含压缩区间——用户仍可回退到压缩点之前）引用的
        # 媒体对象复制进子目录。失败 WARN 不阻断 fork。
        _copy_session_media(store, session_id, new_session_id, child_records)

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
                model_id=source.model_id,
                model_name=source.agent.model,
                provider_name=source.agent.provider_name,
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

        template = AgentTemplate.from_agent(
            source.agent, name=source.template_name, model_id=source.model_id
        )
        new_session = Session.from_template(
            template=template,
            session_id=new_session_id,
            messages=new_messages,
            store=store,
            workspace=source.session_workspace,
        )
        # 先注册 / 刷新计时器再跑 hook（与 create_session 同序）：hook 内 emit 的
        # 事件能命中 SessionReaper 的 touch 订阅，hook 内查 SM 也能看到子会话。
        self._register(new_session)

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

    def session_exists(self, session_id: str) -> bool:
        """会话是否存在（纯解析，不水合、不发事件）。

        与 ``upload_media`` / ``set_session_tags`` 走同一个 ``_resolve_with_store``：
        判据（id 形状 + 内存 + 各 store）与它们看到的完全一致——两个调用点对
        「同一个 id 算不算存在」不能有第二套说法。
        """
        return self._resolve_with_store(session_id) is not None

    # ── 标签 ─────────────────────────────────

    def set_session_tags(
        self,
        session_id: str,
        *,
        add: Iterable[str] = (),
        remove: Iterable[str] = (),
    ) -> TagMutation:
        """按 session id 原子增删标签（幂等；读或写都不触发水合）。

        标签是**持久 metadata**（不是运行时状态），因此有两条互斥路径：已在内存 → 经
        ``Session.apply_tag_ops`` 改内存元数据并落盘（内存对象是磁盘事实的同一来源，绕开它会被后续
        save 回写覆盖）；未加载 / 已逐出 → 直接 store 读改写，**不水合**——给旧会话打 favorite 不会
        把它"弄醒"变成 idle（会话保持 inactive，内存零代价）。

        打标时间（``tag_meta``）随同一处变更维护：新增记时间、移除删记录，与 tags 一次落盘。
        session id 先过格式闸门（``_resolve_with_store``）：不合规的值按"不存在"处理，绝不触达 store
        （防路径穿越）。add / remove 皆空 = 纯读（返回当前标签，不产生任何写）。

        Raises:
            LookupError: 会话不存在（内存与磁盘都没有；id 格式不合规同价）
            ValueError: 标签非法 / 超过单会话上限
        """
        resolved = self._resolve_with_store(session_id)
        if resolved is None:
            raise self.missing_session_error(session_id)
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

    # ── 媒体（上行字节） ─────────────────────

    def upload_media(
        self, session_id: str, data: bytes, *, name: str | None = None
    ) -> MediaRef:
        """把一份媒体字节写进会话媒体目录并返回引用（内容寻址，幂等）。

        上传是"字节 → 存储 → 引用"的用例，落点与 `set_session_tags` 同款：**不水合**
        会话（``_resolve_with_store`` 纯解析）——被逐出的会话不该因一次上传被换入内存
        （`/api/session/list` 的「活跃在前」顺序契约与 `inactive` 痕迹都会被搅动）。
        写入目标是解析出的**磁盘真名**（别名不另起一份会话目录）。id 由字节现算
        （sha256），``name`` 只作展示素材，不参与寻址。

        校验（``wing.media.upload_image_ref``：非空 / 上限 / 支持格式 / 尺寸可解析）全部
        发生在落盘**之前**：失败抛 ``InvalidInputError``，存储零副作用（不建目录、不写
        对象）。上限取生效配置 ``images.max_bytes``（与 ReadImage 同一口径）。

        Raises:
            LookupError: 会话不存在（内存与磁盘都没有；id 格式不合规同价）
            InvalidInputError: 字节不合规（空 / 超上限 / 非支持格式 / 尺寸不可解析）
        """
        resolved = self._resolve_with_store(session_id)
        if resolved is None:
            raise self.missing_session_error(session_id)
        resolved_id, store = resolved

        ref = upload_image_ref(data, max_bytes=get_config().images.max_bytes, name=name)
        store.write_media(resolved_id, ref.id, data)
        return ref

    def read_media(self, session_id: str, media_id: str) -> bytes | None:
        """读取会话媒体对象的字节（B1 读端点；**纯解析 + 存储读，不水合**）。

        与 ``upload_media`` 同款解析：被逐出的会话不该因一次读图被换入内存。字节
        缺失（合法 id 但对象不在）返回 None——两个 404（会话 / 对象）由路由分开报。
        老池回退与物化是 ``store.read_media`` 的既有语义，原样透传。

        Raises:
            LookupError: 会话不存在（内存与磁盘都没有；id 格式不合规同价）
            ValueError: media id 不合规（不是 64 位小写 hex——防路径穿越的闸门）
        """
        resolved = self._resolve_with_store(session_id)
        if resolved is None:
            raise self.missing_session_error(session_id)
        resolved_id, store = resolved
        return store.read_media(resolved_id, media_id)

    # ── 用户文件（上行字节） ─────────────────

    def upload_file(self, session_id: str, data: bytes, *, name: str) -> FileRef:
        """把一份用户文件写进会话文件目录并返回**实际落盘**引用。

        与 ``upload_media`` 同一落点（**不水合**、写入解析出的磁盘真名），差别在
        寻址方式：文件按名寻址且**允许同名多份**（字节不同即 ``-N`` 后缀，绝不覆盖
        ——链上的 ``[file: …]`` 引用指向既有那份）。``name`` 是必填的：agent 与
        用户都靠名字识别这个文件（没有名字的文件无法被引用）。

        校验（名字卫生 / 非空 / ≤ ``files.max_bytes``）全部发生在落盘**之前**：
        失败抛 ``InvalidInputError``，存储零副作用（不建目录、不写字节）。

        Raises:
            LookupError: 会话不存在（内存与磁盘都没有；id 格式不合规同价）
            InvalidInputError: 名字不合规 / 空 body / 超 ``files.max_bytes``
        """
        resolved = self._resolve_with_store(session_id)
        if resolved is None:
            raise self.missing_session_error(session_id)
        resolved_id, store = resolved

        name = validate_file_name(name)
        max_bytes = get_config().files.max_bytes
        if not data:
            raise InvalidInputError("empty request body: expected file bytes")
        if len(data) > max_bytes:
            raise InvalidInputError(file_too_large_message(len(data), max_bytes))
        return store.write_file(resolved_id, name, data)

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

        摘除前同步落一次用量账本：拆解是异步的，进程在它跑完前退出时它就
        不再有机会写盘（lifespan 的收尾 flush 只覆盖当时还在 `_sessions`
        里的会话）——逐出即落盘把「摘除」与「最后一次累计持久化」绑成一步。
        """
        session = self._sessions.get(session_id)
        if session is None:
            return False
        # 先建任务：无运行中的事件循环时在此抛错，状态未被改动（会话保持
        # 在场），不会留下"已摘除却没拆解"的孤儿对象。
        task = asyncio.create_task(self._teardown(session, reason))
        # 摘除即封写：最后一次同步落盘 → 封盘 → 摘除（顺序即语义）。此后
        # 迟到的落盘（teardown 收尾 / 节流重试）都是 no-op——否则会把 live
        # 目录写回来（归档场景下的数据分裂红线，见 Session.detach）。
        session.flush_usage()
        session.detach()
        self._sessions.pop(session_id, None)
        self._last_active.pop(session_id, None)
        self._teardowns.add(task)
        task.add_done_callback(self._teardowns.discard)
        return True

    async def _teardown(self, session: Session, reason: str) -> None:
        """拆解会话运行期资源并记一行日志（异常不逃逸）。

        拆解跑在独立任务里（reaper sweep / release 端点都不携带上下文）——
        显式绑定 session，让拆解链路的日志自动带上归属。
        """
        with session_context(session.session_id):
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
                raise self.missing_session_error(session_id)
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

    def flush_usage(self) -> None:
        """把所有在场会话的用量账本落盘（进程退出前的最后落盘点）。

        回合收尾与逐出路径各自已落；这里兜住「回合进行中进程退出」的尾巴
        ——最后一次累计也是跨重启延续的一部分。已落过的会话是 no-op。
        """
        for session in self._sessions.values():
            session.flush_usage()

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
    # 外部方法：归档（archive）
    # ============================================================

    def archive_session(self, session_id: str) -> tuple[bool, str]:
        """把会话移入归档区（幂等；用户显式动作 / sweep 共用本入口）。

        归档区是**冷存储**：归档后的会话从列表消失、不可解析（resume / send /
        tag 一律"已归档"），恢复必须显式 :meth:`unarchive_session`。移动是纯
        rename——不改任何字节，归档只是换了位置。

        钉住条件与 :meth:`release_session` 同一套（忙碌 / 待处理输入 / 订阅中 /
        非归档后端）；**空闲可收回**的会话会先逐出再移动：归档必然要让它离开
        内存，而且"先摘除、再 rename"是同一事件循环轮次里的同步两步——此窗口内
        不可能有水合/订阅插入（否则活 Session 会把目录写回 live 区，数据分裂）。

        Returns:
            (archived, detail)：archived=False 是幂等 / 空动作（不是错误）——
            已在归档区（"already archived"）或**不构成"会话"**的目录（"nothing
            to archive"：空会话 / 只写过 metadata 未发言未打标 / 只有媒体——
            按展示判据它们本来就不出现在列表里）；detail ∈ {"archived",
            "already archived", "nothing to archive"}。

        Raises:
            LookupError: live 区与归档区都没有该会话
            RuntimeError: 钉住（忙碌 / 待处理输入 / 订阅中）、后端不支持归档、
                或 live 与归档区同时存在（手工拷贝等异常态）
        """
        resolved = self._resolve_with_store(session_id)
        if resolved is None:
            if self._archived_store(session_id) is not None:
                return False, "already archived"
            raise LookupError(f"Session not found: {session_id!r}")

        resolved_id, store = resolved
        if not store.archivable:
            raise RuntimeError(f"store '{store.name}' does not support archiving")

        if not store.is_listed_session(resolved_id):
            # 准入判据 = **展示判据**（history 或带标签的 metadata，与列表 /
            # 归档区重建同一口径）：只有"会出现在 ps 里的会话"才值得归档。
            # - 内存中的空会话（从未发言 → 磁盘零痕迹）：没有可归档的磁盘状态；
            # - 只写过 metadata（模型绑定 / workspace 等）却没发言、没标签的目录：
            #   它本来就不出现在任何列表里，归档它只会造出一个"索引重建后不可
            #   发现"的归档条目；
            # - 只传过媒体的会话同档（media 目录留在 live 区不动）。
            # 如实回答空动作，而不是让 store 抛 FileNotFoundError（500）。
            return False, "nothing to archive"

        # 双存在预检**先于**任何副作用：否则 evict 之后 store 再抛（双存在 /
        # rename 竞态）会留下"已逐出但没归档"的半截状态（无回滚）。
        if store.resolve_archived_id(resolved_id) is not None:
            raise RuntimeError(
                f"session '{resolved_id}' exists in both live and archive dirs"
            )

        session = self._sessions.get(resolved_id)
        if session is not None:
            blocker = self._blocked_reason(session)
            if blocker is not None:
                raise RuntimeError(f"session cannot be archived: {blocker}")
            self.evict(resolved_id, reason="archived")
        try:
            store.archive(resolved_id)
        except OSError as e:
            # 移动失败（rename / 建目录）：内存态已摘除，磁盘照旧（会话仍可
            # 水合，重跑归档即可修复）——转成 RuntimeError 让路由回 409 而不是
            # 500，并把原因说清楚（半副作用是内存态，不是数据）。
            raise RuntimeError(
                f"session '{resolved_id}' could not be moved to the archive: {e}"
            ) from e
        log.info(f"Session archived: {resolved_id}")
        return True, "archived"

    def unarchive_session(self, session_id: str) -> tuple[bool, str]:
        """把会话从归档区移回 live 区（幂等）。

        恢复会**刷新 last_interaction**（显式动作 = 兴趣信号）：否则自动 sweep
        会按"归档前的旧年龄"在下一轮把它立刻收回去（恢复后只看 tail / ps 的
        用户会觉得会话又凭空消失）。这是恢复方向的唯一写操作（改一个字段）；
        归档方向仍是纯 rename（字节零变化）。

        Returns:
            (restored, detail)：restored=False 表示不在归档区（幂等 no-op——
            会话在 live 区存在即这一种）；detail ∈ {"restored", "not archived"}。

        Raises:
            LookupError: live 区与归档区都没有该会话
            RuntimeError: live 区已有同名会话（双存在异常态，不覆盖）
        """
        live = self._resolve_with_store(session_id)
        archived: tuple[SessionStore, str] | None = None
        if is_valid_session_id(session_id):
            for store in self._stores.values():
                canonical = store.resolve_archived_id(session_id)
                if canonical is not None:
                    archived = (store, canonical)
                    break

        if archived is not None and live is not None:
            raise RuntimeError(
                f"session {session_id!r} exists in both live and archive dirs"
            )
        if archived is not None:
            store, canonical = archived
            _warn_alias(session_id, canonical)
            try:
                store.unarchive(canonical)
            except OSError as e:
                # 与归档方向对称：移动失败（权限 / IO）转 RuntimeError → 409，
                # 而不是让 OSError 逃逸成 500（两个方向的失败类该长一样）。
                raise RuntimeError(
                    f"session '{canonical}' could not be moved back from the "
                    f"archive: {e}"
                ) from e
            # 恢复是显式动作 = 兴趣信号：刷新 last_interaction，否则自动 sweep
            # 下一轮（启动 +startup_delay 或每天）会按旧年龄把它立刻收回去，
            # 用户"刚恢复的会话又消失"。落盘失败只 WARN——移动已完成，
            # 报错只会让调用方以为没恢复（脏状态自愈路径：下一轮 sweep 再看）。
            self._refresh_last_interaction(store, canonical)
            log.info(f"Session restored from archive: {canonical}")
            return True, "restored"
        if live is not None:
            return False, "not archived"
        raise LookupError(f"Session not found: {session_id!r}")

    def _refresh_last_interaction(self, store: SessionStore, session_id: str) -> None:
        """把会话的 last_interaction 拨到现在（恢复路径专用；失败只 WARN）。"""
        try:
            metadata = store.load_metadata(session_id) or SessionMetadata()
            metadata.last_interaction = datetime.now().isoformat()
            store.save_metadata(session_id, metadata)
        except Exception as e:  # noqa: BLE001 —— 恢复已完成，落盘失败不反悔
            log.warning(
                f"Session {session_id}: failed to refresh last_interaction "
                f"after restore: {e}"
            )

    def list_archived_sessions(self) -> list[ArchivedSession]:
        """列出归档区条目（跨后端聚合；读索引，不扫会话目录）。

        投影口径与 ``list_sessions`` 对齐：tags 清洗后投影；模型四件套经
        ``resolve_model_binding`` 现算（id 命中走当前映射、未命中回落归档快照、
        再退模板默认——归档条目的"在跑什么模型"与 ps 用同一条链读）。

        排序：最后交互时间降序（未知排最后）→ id 升序定序——与 list_sessions
        同一精神：并列项顺序不能取决于索引文件的物理顺序。
        """
        config = get_config()
        result: list[ArchivedSession] = []
        for store in self._stores.values():
            if not store.archivable:
                continue
            for entry in store.list_archived():
                binding = resolve_model_binding(
                    SessionMetadata(
                        model_id=entry.model_id,
                        model_name=entry.model_name,
                        provider_name=entry.provider_name,
                        template_name=entry.template_name,
                    ),
                    config,
                    self._template_or_default(entry.template_name),
                )
                result.append(
                    entry.model_copy(
                        update={
                            "tags": sanitize_tags(entry.tags),
                            "model_id": binding.model_id,
                            "model_name": binding.model_name,
                            "provider_name": binding.provider_name,
                            "model_display_name": binding.model_display_name,
                        }
                    )
                )
        result.sort(
            key=lambda e: (-(_session_epoch(e.id, e.last_interaction) or 0.0), e.id)
        )
        return result

    async def archive_stale_sessions(self, idle_days: float) -> list[str]:
        """归档所有「超过 ``idle_days`` 未交互」的会话，返回被归档的 id 列表。

        与逐出（纯内存判定）不同：归档要碰磁盘、成本 O(会话数)，因此分两阶段：

        ① **线程内**枚举候选（读 metadata 判年龄；见 :meth:`_collect_stale`）——
        这一阶段在事件循环上会卡住网关（流式 token / 请求响应全被拖住）；
        ② **事件循环上**逐个终检（重读磁盘事实）并复用 :meth:`archive_session`
        完成「逐出 + 移动」（显式归档路径；sweep 只碰不在内存里的会话）——移动必须与 resume / subscribe 等同步路径同线程
        顺序执行，否则"目录被搬走时刚好在加载"会留下幽灵目录（数据分裂）。

        ``idle_days <= 0`` = 关闭（关闭自动归档；显式归档不受影响）；带 ``pin``
        标签的会话豁免；钉住条件命中（忙碌 / 订阅 / 待处理输入）跳过本轮。
        """
        if idle_days <= 0:
            return []
        candidates = await asyncio.to_thread(self._collect_stale, idle_days)
        archived: list[str] = []
        with ExitStack() as index_batches:
            # 索引批量写：n 条候选只读改写一次（不做的话每条都要读+写整份
            # index.json，O(n²) 且 n 次 fsync）。目录移动仍逐条即时、原子。
            for store in self._distinct_stores(candidates):
                index_batches.enter_context(store.index_batch())
            for store, session_id in candidates:
                try:
                    if not self._still_stale(store, session_id, idle_days):
                        continue  # 期间被交互 / 被打了 pin / 已消失 / 已归档
                    changed, _ = self.archive_session(session_id)
                    if changed:
                        archived.append(session_id)
                except (LookupError, RuntimeError) as e:
                    # 钉住 / 双存在 / 竞态消失：跳过本轮，下一轮再判定
                    log.warning(f"Archive sweep: skipping {session_id!r}: {e}")
                except OSError as e:
                    log.error(f"Archive sweep: {session_id!r} failed: {e}")
                finally:
                    # 批量 sweep 的候选可能上千（老用户升级后的第一轮）：候选之间
                    # 让出事件循环，否则整批是一个同步长块——所有会话的流式输出与
                    # 请求全停。单条候选内部（终检 → 逐出 → rename）仍是同一轮次，
                    # 原子性不受影响。
                    await asyncio.sleep(0)
        return archived

    @staticmethod
    def _distinct_stores(
        candidates: list[tuple[SessionStore, str]],
    ) -> list[SessionStore]:
        """候选里出现过的存储（保持出现顺序；索引批量窗口按 store 开一次）。"""
        seen: dict[int, SessionStore] = {}
        for store, _ in candidates:
            seen.setdefault(id(store), store)
        return list(seen.values())

    def _collect_stale(self, idle_days: float) -> list[tuple[SessionStore, str]]:
        """（阶段①，线程内调用）枚举「超过阈值未交互」的候选（纯读）。

        判据：是会话（``list_summaries`` 口径）、年龄**已知且**超过阈值——
        ``_session_epoch`` 为 None（时间取不到）的**跳过**：不发明时间，保守
        不动未知会话。pin 豁免在这里初筛（省掉阶段②的重读），但结论以阶段②
        的终检为准（见 :meth:`_still_stale`）。
        """
        threshold = time.time() - idle_days * 86400.0
        candidates: list[tuple[SessionStore, str]] = []
        for store in self._stores.values():
            if not store.archivable:
                continue
            for summary in store.list_summaries():
                epoch = _session_epoch(summary.id, summary.metadata.last_interaction)
                if epoch is None or epoch >= threshold:
                    continue
                if PIN_TAG in sanitize_tags(summary.metadata.tags):
                    continue
                candidates.append((store, summary.id))
        return candidates

    def _still_stale(
        self, store: SessionStore, session_id: str, idle_days: float
    ) -> bool:
        """（阶段②，事件循环上调用）终检候选是否**仍然**应该归档。

        重读磁盘事实：阶段①到②之间有真实的等待窗口（线程调度 / 事件循环让出），
        期间的交互（新消息、打 pin、甚至已归档）必须让候选当场失效——归档是
        破坏可见性的动作，不能按过期的快照执行。

        **在内存里的一律不动**：sweep 只管"不在工作集里"的会话（内存 tier 的
        回收是逐出 job 的职责）。这条规则的两面都必要：用户刚 resume 打开的
        旧会话不能被后台从脚下收走（resume 刻意不写 metadata——"空转 resume
        零写盘"是既有不变量——所以不能靠刷新年龄保护它）；而还在被前端订阅 /
        正忙的会话本来也不该被动。会话被逐出后重新成为磁盘会话，下一轮 sweep
        按它的磁盘年龄照常处理。
        """
        if session_id in self._sessions:
            return False
        metadata = store.load_metadata(session_id)
        if metadata is None:
            return False  # 已消失（手工删除等）
        if PIN_TAG in sanitize_tags(metadata.tags):
            return False
        epoch = _session_epoch(session_id, metadata.last_interaction)
        if epoch is None:
            return False
        return epoch < time.time() - idle_days * 86400.0

    # ============================================================
    # 外部方法：查询
    # ============================================================

    def list_sessions(
        self,
        *,
        statuses: Collection[str] | None = None,
        tags: Sequence[str] = (),
        parent: str | None = None,
    ) -> list[SessionInfo]:
        """列出所有有效 session（跨 stores 聚合），按「活跃优先 + 最后交互时间降序」。

        每个 session 携带运行时 `status`：已加载进内存（在 `self._sessions` 中）→ 取 live 状态
        （idle/working/waiting）；未 resume → `inactive`。

        排序口径（唯一事实来源，前端按原序渲染、不再重排）：① `status != "inactive"` 的（= 已在
        内存里的工作集）在前，未加载的在后；② 组内按 `_session_epoch`（最后一次交互时间）降序；
        ③ 完全并列（含都取不到时间）时按 session id 升序——**只为定序**，不是优先级：没有它，并列
        项的先后就取决于 store 的枚举顺序（`iterdir()` / SQL 返回序），同一个列表两次请求可能给出
        不同顺序。

        `status` 只用来区分 active / inactive，不再有组内优先级：`waiting`（正在等用户回答）不因为
        状态本身提前——需要突出 waiting 时看面板上的状态图标（`?`）。workspace 也不参与排序。

        模型四件套（`model_id` / `model_name` / `provider_name` / `model_display_name`）与 `status`
        同源同刻地投影：在内存的会话取 live agent（`Session.model_binding`），未加载的按 resume 链
        解析盘上记录（`resolve_model_binding`）。两条路都不发网络请求、不建会话。

        `last_turn_end` / `last_turn_subtype` 取自 `summary.metadata`（冷读磁盘，与是否存在内存无关）
        ——「已完成」收件箱要的就是「未加载 / 已被逐出的会话也带着最近一次收口事实」；旧会话 /
        没收口过的会话为 None。

        ``statuses`` / ``tags`` / ``parent`` 是**子集过滤**（缺省不过滤，同时给出即 AND）：

        - ``statuses``：只保留状态落在集合内的行（``None`` = 不过滤；空集合 = 无行）。
          「在内存的会话」即 ``statuses={"idle","working","waiting"}``。
        - ``tags``：只保留**携带全部**给定标签的行（AND，与 ``wing ps --tag`` 的
          既有标签口径一致）；标签按字面比对（写侧已保证不存在的标签不可能命中，
          滤它是空结果，不是错误）。
        - ``parent``：只保留 **parent_session_id 逐字等于**该值的行（直接子——不含
          父自己、不含孙辈）。血缘是弱引用，值只做字面比对：**格式合法但不存在的
          父**是空结果（无从枚举、也不该报错），词法非法由路由层折成 400。

        过滤发生在**可见性规则之后**：无名且无标的会话本就不进列表（无论状态如何），
        过滤不会让它们出现——过滤只做减法，不造行。

        过滤发生在聚合之后：结果是**全量列表的子序列**（同样的行、同样的相对顺序，
        只是少了几行）——调用方可以按"全量 + 谓词"理解它。服务端成本与不过滤时
        相同（聚合本身即既有的一次跨 store 遍历），省下的是响应体积与调用方的
        清洗/合并成本。
        """
        config = get_config()
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
                binding = (
                    loaded.model_binding()
                    if loaded is not None
                    else resolve_model_binding(
                        metadata,
                        config,
                        self._template_or_default(metadata.template_name),
                    )
                )
                row_tags = sanitize_tags(metadata.tags)
                result.append(
                    SessionInfo(
                        id=summary.id,
                        name=name,
                        workspace=metadata.workspace,
                        parent_session_id=metadata.parent_session_id,
                        last_interaction=metadata.last_interaction,
                        last_turn_end=metadata.last_turn_end,
                        last_turn_subtype=metadata.last_turn_subtype,
                        status=loaded.status if loaded is not None else "inactive",
                        tags=row_tags,
                        tag_meta=sanitize_tag_meta(metadata.tag_meta, row_tags),
                        model_id=binding.model_id,
                        model_name=binding.model_name,
                        provider_name=binding.provider_name,
                        model_display_name=binding.model_display_name,
                    )
                )

        if statuses is not None:
            result = [s for s in result if s.status in statuses]
        if tags:
            result = [s for s in result if all(t in s.tags for t in tags)]
        if parent is not None:
            result = [s for s in result if s.parent_session_id == parent]

        # 活跃优先（False < True），组内时间新的在前；并列时按 id 定序（见 docstring）。
        result.sort(
            key=lambda s: (
                s.status == "inactive",
                -(_session_epoch(s.id, s.last_interaction) or 0.0),
                s.id,
            )
        )

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
        media: list[MediaRef] | None = None,
    ) -> None:
        """路由消息到指定 session。（内部方法，由 WingRuntime 调用）

        Contextvars 由 WingRuntime.post() 统一管理，此方法不设置/恢复。

        - 不在内存的会话先按需水合（逐出后仍可投递）
        - 携带的媒体引用先过闸门（id 格式 + 对象存在）——不可兑现即拒（幽灵引用
          不进历史，也**没有**送达 ack：拒绝发生在 DeliveredEvent 之前）
        - / 开头且匹配 prompt 命令 → 展开为纯文本后投递
        - 否则 → 直接投递给 session.post()

        Raises:
            LookupError: 内存与磁盘都没有该会话（调用方负责映射错误面）
            InvalidInputError: 携带的媒体引用不可兑现（id 非法 / 对象不存在）
        """
        assert session_id is not None, "session_id is required by WingRuntime"

        session = self.ensure_loaded(session_id)
        validate_media_refs(session.store, session.session_id, media)

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
            "SM._post: routing '%s' to session.post (session=%s)",
            log_excerpt(content, 50),
            session_id,
        )
        await session.post(
            content,
            request_id=request_id,
            tool_call_id=tool_call_id,
            media=media,
        )
