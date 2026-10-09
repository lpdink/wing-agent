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

from dataclasses import dataclass, field
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
    SettingsChangedEvent,
    SyncSessionEvent,
    WingEvent,
)
from wing.common.fs import atomic_write_bytes, atomic_write_text
from wing.common.logger import log
from wing.event_bus import event_bus
from wing.config import (
    Config,
    ConfigProblem,
    ModelGroup,
    build_catalog,
    cross_field_problems,
    emit_config_yaml,
    get_config,
    get_config_path,
)
from wing.config.document import (
    ConfigDocumentError,
    ConfigFingerprint,
    SparseDocument,
    changed_paths,
    locate_problems,
    merge_with_defaults,
    read_document,
    resolve_secrets,
    restart_required_paths,
)
from wing.hooks import load_hooks
from wing.request_context import (
    get_request_context,
    reset_request_context,
    set_request_context,
)
from typing import TYPE_CHECKING, Any
from collections.abc import Iterable
from wing.session import Session, SessionManager, SessionReaper, TagMutation
from wing.system import ReloadResult, reload_system as _reload_system
from wing.store import FileSessionStore, MemorySessionStore, SessionStore

if TYPE_CHECKING:
    from wing.session import AgentOverride, AgentTemplateManager


# ============================================================
# Setting API —— 保存事务的数据形状（L4：编排结果，不是 wire 模型）
# ============================================================


UNPARSEABLE_CONFIG_WARNING = "原配置文件无法解析，其中的密钥无法保留，请重新填写"
"""当前 ``config.yaml`` 读不出文档时的回执警告（AD13）。

语法错意味着我们不知道文件里原本有什么：旧密钥无从回填（``null`` 不再等于「保留」），
未知键也无从保留。回执必须**显式**说明这一点——用户按面板提示重填密钥，而不是
在保存后静默失去一个 provider 的凭据。
"""

DROPPED_SECRET_WARNING = "无法确定 {path} 属于哪一项（列表结构变化且无法按身份配对），已移除该密钥，请重新填写"
"""密文哨兵因无法安全配对而被丢弃时的回执警告（审查 A1 的第 3 点）。

比错配轻、比静默重：**宁可不猜**——列表删 / 移 / 前插后配不上的密钥一律移除
（必填字段随之成为 problem，保存被拦住），并在回执里点名路径 + 要求重填；
绝不把 A 的密钥按旧下标写给 B。逐条（一条路径一条）与面板 / CLI 的逐条渲染对齐。
"""


class SettingsConflictError(RuntimeError):
    """保存的基线指纹与磁盘现状不符（乐观并发冲突）。路由据此回 409。

    携带磁盘当前指纹：客户端拿不到它就得先 GET 一次才能重试；409 的 detail 直接带上，
    重试路径缩短一步。
    """

    def __init__(self, fingerprint: str) -> None:
        super().__init__(
            f"config.yaml changed on disk (current fingerprint: {fingerprint})"
        )
        self.fingerprint = fingerprint


@dataclass
class SettingsApplyResult:
    """``apply_settings`` 的回执（领域形状；wire 投影见 ``gateway/projection.py``）。

    ``ok=False`` 时 ``fingerprint`` 是**磁盘当前指纹**（文件一个字节都没写）；
    ``setup_mode_exited`` 为真 ⟺ 这次保存把网关从 setup mode 推进了正常模式
    （04；正常模式下恒 ``False``）。
    """

    ok: bool
    fingerprint: str
    problems: list[ConfigProblem] = field(default_factory=list)
    changed: list[str] = field(default_factory=list)
    restart_required: list[str] = field(default_factory=list)
    reload: ReloadResult | None = None
    setup_mode_exited: bool = False
    backup_path: str | None = None
    warnings: list[str] = field(default_factory=list)
    """非致命的告知（AD13）：如「原配置文件无法解析，其中的密钥无法保留」。
    与 ``problems`` 的区别：problems 让保存失败（``ok=False``），warnings 只是提醒。"""


@dataclass
class WriteEffectResult:
    """保存事务第 ⑦ 步（「生效」）的产物——04 引入的接点形状。

    ``reload`` 是逐项结果：

    - 正常模式 = 热重载六项（``reload_system``：config.yaml / hooks / prompt commands /
      provider / skills & rules / log level）；
    - setup mode = 转入正常模式的六步（``GatewayServer._enter_operational``：
      config.yaml / log level / prompt commands / runtime / background jobs / auth）。

    两条路径永不混用（前者是「已经在跑，换一份配置」，后者是「从无到有」），
    回执形状相同（``ReloadResponse``）。
    """

    reload: ReloadResult
    setup_mode_exited: bool = False


async def settings_write_effect(sm: SessionManager) -> ReloadResult:
    """保存写盘后的「生效」步骤（§7.3 第 ⑦ 步）的正常模式实现体。

    热重载（config.yaml → hooks → prompt commands → provider → skills & rules → log level）。
    唯一调用点：``WingRuntime.post_write_effect()``（04 把这个接点从模块函数上移成
    实例方法，见那里的说明）。失败**不回滚文件**——配置本身是合法的，回执按
    ``ReloadResult`` 的逐项 ok 如实报告（与 ``/api/system/reload`` 同语义）。
    """
    return await _reload_system(sm)


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
        tags: list[str] | None = None,
        session_id: str | None = None,
    ) -> Session:
        """创建（或按 id 收养）session。

        Args:
            backend: 存储后端（file/memory），None 使用默认后端（file）。
            tags: 创建即打标（校验语义同 :meth:`set_session_tags`）。
            session_id: 指定 id 的 **create-or-adopt**（None = 自生成；
                已存在则收养既有会话，语义同 :meth:`resume_session`）。

        Raises:
            ValueError: session_id 不合规 / 模板不存在 / backend 未知 / 标签非法
        """
        return self.sm.create_session(
            template_name=template_name,
            workspace=workspace,
            agent_override=agent_override,
            backend=backend,
            tags=tags,
            session_id=session_id,
        )

    def resume_session(
        self, session_id: str, agent_override: AgentOverride | None = None
    ) -> Session:
        """从磁盘恢复已有 session（精确匹配 + 闸门，见 SessionManager）。

        Args:
            agent_override: resume 语义的参数覆盖（只应用
                model_id/effort/tools 子集，见 `Session.apply_resume_override`）

        Raises:
            LookupError: session 不存在（含 id 不过闸门——与本处同价）
            ValueError: 覆盖里的 model_id 未命中 / 工具引用无法解析
        """
        return self.sm.resume_session(session_id, agent_override=agent_override)

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

    def set_session_tags(
        self,
        session_id: str,
        *,
        add: Iterable[str] = (),
        remove: Iterable[str] = (),
    ) -> TagMutation:
        """原子增删会话标签（add / remove 皆空 = 纯读；不水合已逐出会话）。

        Raises:
            LookupError: session 不存在
            ValueError: 标签非法 / 超过单会话上限
        """
        return self.sm.set_session_tags(session_id, add=add, remove=remove)

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
            options=session.agent.request_options(),
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

    async def interrupt_session(self, session_id: str) -> None:
        """中断 session 当前 agent 任务。

        agent.interrupt() 内部有界等待旧 worker 完成补提交（流式半截
        reasoning/text 的 partial Message 已落盘），之后 InterruptedEvent
        才落盘+广播——链序保证事件在 partial Message 之后。worker 在取消
        阶梯内始终不终止时（极端形态），interrupt 在阶梯耗尽（~18s）后
        保留旧 worker 并返回，InterruptedEvent 仍会发出；该退化路径由
        agent 侧 ERROR 日志与 notice 标注。

        事件携带打断入口放弃的积压输入 `request_id` 列表（前端据此只丢弃
        真正被放弃的 pending 消息——锁等待期间新到的消息幸存）。

        Raises:
            LookupError: session 不存在
        """
        session = self._require_session(session_id)
        dropped_request_ids = await session.agent.interrupt()
        self._emit_session_event(
            InterruptedEvent(
                session_id=session.session_id,
                dropped_request_ids=dropped_request_ids,
            ),
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
        model_id: str | None = None,
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

        Args:
            model_id: 切换模型（引用 providers[].models 的 id；未命中 ValueError）

        Raises:
            LookupError: session 或 template 不存在
            ValueError: model_id 未命中 id 空间 / workspace 路径不合法 /
                工具引用无法解析
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
            model_id=model_id,
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
        model_emitted = model_id is not None or agent_switched
        self._emit_session_event(
            SessionStateChangedEvent(
                session_id=session.session_id,
                model=session.agent.model if model_emitted else None,
                model_id=session.model_id if model_emitted else None,
                provider_name=session.agent.provider_name if model_emitted else None,
                model_display_name=session.agent.model_display_name
                if model_emitted
                else None,
                thinking=session.agent.thinking
                if thinking is not None or agent_switched
                else None,
                reasoning_effort=session.agent.reasoning_effort
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

    def list_models(self) -> list[ModelGroup]:
        """模型目录（按 provider 分组，配置声明序）。

        目录是**配置声明的同步投影**——不查远端、不发网络请求（远端 ``/models``
        发现已退役）。gateway 路由经此获取，不感知 config。
        """
        return get_config().model_groups()

    async def reload_system(self) -> ReloadResult:
        """热重载全局配置、hooks、prompt commands、provider、skills & rules、log level。

        config 加载失败时立即中止。其余项失败时继续。

        流程本体在 ``wing/system.py``（11 归位：逐字自本方法抽出，步骤顺序与
        失败语义不变）——本方法只做委托，调用点（gateway 路由）不变。
        """
        return await _reload_system(self.sm)

    # ============================================================
    # Setting API —— 保存事务（唯一写盘路径）
    # ============================================================

    async def post_write_effect(self) -> WriteEffectResult:
        """保存写盘后的「生效」步骤（§7.3 第 ⑦ 步）——**04 的接点**。

        唯一调用点是 :meth:`apply_settings` 的第 ⑦ 步。

        - 正常模式（本类）：热重载（:func:`settings_write_effect` 的六项逐项结果）。
        - setup mode（04）：``gateway.server._SetupRuntime`` 覆盖本方法——换成
          「转入正常模式」（``GatewayServer._enter_operational()``），并把
          ``setup_mode_exited`` 置真。

        把接点从模块级函数（03 的 ``settings_write_effect``）上移成**实例方法**，
        是因为「setup mode 下的 runtime」是 ``WingRuntime`` 的替身（见
        ``gateway/server.py`` 的 ``_SetupRuntime``）——覆盖一个方法是替身唯一的
        介入点，而事务本体（①–⑥、⑧–⑩）因此只有一份实现。失败**不回滚文件**：
        配置本身合法，回执按 ``ReloadResult`` 逐项如实报告。
        """
        return WriteEffectResult(reload=await settings_write_effect(self.sm))

    async def apply_settings(
        self, document: dict[str, Any], base: str | None = None
    ) -> SettingsApplyResult:
        """保存事务（总设计 §7.3 的十步）：校验 → ``.bak`` → 原子写 → 生效 → 事件 → 回执。

        ① 现读磁盘（服务端不缓存文档）→ ② 基线指纹不符 → 409（不写盘）→
        ③ 密文三态回填（``null`` = 保留磁盘现值）→ ④ 字段级 + 跨字段校验
        （**全有或全无**：有 problem 时一个字节都不写）→ ⑤ ``config.yaml.bak``
        （覆盖式，只留最近一份）→ ⑥ 规范形 YAML + 原子写 → ⑦ 生效
        （:meth:`post_write_effect`，04 的接点；失败不回滚文件）→
        ⑧ ``changed`` / ``restart_required``（只读叶子的 apply，增补 P13）→
        ⑨ 广播 ``SettingsChangedEvent``（global）→ ⑩ 回执。

        Args:
            document: 稀疏文档（密文三态见 ``resolve_secrets``）；未知键原样保留
                （从磁盘文档回填，客户端不认识也不会吃掉它）。
            base: 客户端持有的基线指纹（``None`` = 跳过并发检查，对应 CLI 的 ``--force``）。

        Raises:
            SettingsConflictError: 指纹不匹配（乐观并发）——文件未被触碰。
        """
        catalog = build_catalog()

        # ① 现读磁盘（D18：不缓存）。
        warnings: list[str] = []
        try:
            current_doc, current_fp = read_document()
        except ConfigDocumentError as exc:
            # 文件存在但读不出文档（YAML 语法错 / 顶层不是映射）。**事务照常继续**：
            # 这正是「配置写坏 ⇒ 开设置面板修」（D1）在语法错这条分支上的兑现——
            # 否则面板保存永远 ok=false、用户只剩手改文件一条路（审查 B3 / AD13）。
            # 现文档视为**空**：未知键无从保留、旧密钥无从回填（回执里显式警告）。
            # 指纹仍然算（= 文件字节的 sha256）——乐观并发不因文件坏了而失效。
            current_doc = SparseDocument(data={})
            current_fp = ConfigFingerprint(value=exc.fingerprint)
            warnings.append(UNPARSEABLE_CONFIG_WARNING)

        # ② 乐观并发：指纹不匹配 → 409（不写盘）。
        if base is not None and base != current_fp.value:
            raise SettingsConflictError(current_fp.value)

        # ③ 密文回填：null = 保留磁盘现值（真实值只在这里被读、从不回显）。
        #    列表项按**身份**配对（identity_field）；配不上且长度变化 → 宁可不猜：
        #    该密钥被丢弃（必填 → problem），并且**必须让用户看见**（审查 A1：
        #    静默错配是数据损坏级的——A 的密钥写给 B，回执不报告）。
        resolution = resolve_secrets(
            SparseDocument(data=document), current_doc, catalog
        )
        incoming = resolution.document
        warnings.extend(
            DROPPED_SECRET_WARNING.format(path=path)
            for path in resolution.dropped_secrets
        )

        # ④ 校验（字段级 + 跨字段）：有 problem 就到此为止——全有或全无。
        raw = merge_with_defaults(incoming, catalog)
        problems = locate_problems(raw) + cross_field_problems(
            Config.model_construct(**raw)
        )
        if problems:
            return SettingsApplyResult(
                ok=False,
                fingerprint=current_fp.value,
                problems=problems,
                warnings=warnings,
            )

        # ⑤ 备份（存在才备份；覆盖式：只留最近一份）。原子写（不是 copyfile）：
        # 进程在拷贝中途被杀不会留下截断的 .bak（审查 N7）；字节级复制，
        # 连读不出文档的坏文件也逐字留证（AD13 的价值所在）。
        config_path = get_config_path()
        backup_path: str | None = None
        if config_path.exists():
            backup = config_path.with_name(f"{config_path.name}.bak")
            atomic_write_bytes(backup, config_path.read_bytes())
            backup_path = str(backup)

        # ⑥ 规范形 YAML（未知键取自磁盘文档，D17；读不出文档时没有未知键可保留）
        #    + 原子写。
        text = emit_config_yaml(incoming.data, catalog, extra=current_doc.extra)
        atomic_write_text(config_path, text)
        _, new_fp = read_document()

        # ⑦ 生效（04 的接点；失败不回滚——配置本身合法，回执逐项报告）。
        effect = await self.post_write_effect()

        # ⑧ 差异 → changed / restart_required（P13：只读叶子路径的 apply）。
        changed = changed_paths(current_doc, incoming, catalog)
        restart = restart_required_paths(changed, catalog)

        # ⑨ 广播（global scope：所有客户端都该知道配置变了）。日志只写 path，不写值。
        log.info(
            f"settings changed: paths={changed} restart_required={restart} "
            f"fingerprint={current_fp.value}→{new_fp.value}"
        )
        event_bus.emit(
            SettingsChangedEvent(
                changed=changed,
                restart_required=restart,
                fingerprint=new_fp.value,
                target=EventTarget(scope="global"),
            )
        )

        # ⑩ 回执。
        return SettingsApplyResult(
            ok=True,
            fingerprint=new_fp.value,
            changed=changed,
            restart_required=restart,
            reload=effect.reload,
            setup_mode_exited=effect.setup_mode_exited,
            backup_path=backup_path,
            warnings=warnings,
        )

    # ============================================================
    # 内部辅助
    # ============================================================

    def _require_session(self, session_id: str) -> Session:
        """获取 session，不存在则 raise LookupError。"""
        session = self.sm.get_session(session_id)
        if session is None:
            raise LookupError(f"Session not found: {session_id!r}")
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
