# wing/agent/core.py
"""WingAgent — 瘦壳：组装 + 对外接口 + worker 生命周期。

实现 ToolContext Protocol，供工具侧通过窄接口访问 agent 能力。
内部编排委托给 ReActLoop / ToolExecutor / AgentEventSink / Inbox。
"""

from __future__ import annotations

import asyncio
import functools
import inspect
import time
import uuid
from collections.abc import Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any

from wing.common.logger import log
from wing.config import (
    ModelCapabilities,
    get_config,
    resolve_model_capabilities,
    resolve_model_display_name,
)
from wing.event import AskEvent, WingEvent
from wing.diagnostics import (
    InterruptLockWatch,
    log_cancel_snapshot,
    stack_trace,
    task_label,
    watch_undead_task,
)
from wing.provider import create_provider
from wing.provider.base import ModelProvider
from wing.schema import Message, Tool

from .event_sink import AgentEventSink
from .inbox import Inbox
from .react_loop import ReActLoop
from .tool_executor import ToolExecutor, _current_tool_call_id

if TYPE_CHECKING:
    from wing.media import MediaAccess


class WingAgent:
    """Agent 核心——组装各部件，提供对外接口。"""

    def __init__(
        self,
        model: str,
        model_provider: ModelProvider,
        context_manager: Any,  # ContextManager（避免循环导入）
        stream: bool = False,
        tools: list[Tool] | None = None,
        max_turns: int | None = None,
        yolo: bool | None = None,
        media: MediaAccess | None = None,
    ) -> None:
        self.model = model
        self.model_provider = model_provider
        self.context_manager = context_manager
        self.stream = stream
        self._media = media
        """会话媒体读写窄接口（工具写图 / provider 序列化读图）。

        为 None 表示该 agent 没有媒体存储（测试构造的裸 agent）——工具侧
        必须据此安全拒绝（不得假定可用）。同一 MediaAccess 实例可被多个
        agent 共享（共享存储池，不复制字节）。"""

        # Provider client 表：按 name 有界持有，切回同名复用（创建即拥有）。
        # 跨 provider 切模型不关闭旧 client（不打断在途生成）；shutdown() 不动
        # provider——client 生命周期由其创建方终结；aclose_providers() 仅在
        # agent 整体废弃（模板切换）或 session 释放时调用。
        self._providers: dict[str, ModelProvider] = {
            model_provider.name: model_provider
        }

        # ── 内部部件组装 ──
        self._inbox = Inbox()
        self._sink = AgentEventSink(
            session_id=self.session_id,
            append_event=context_manager.append_event,
        )
        self._executor = ToolExecutor(self._sink)
        self._loop = ReActLoop(
            tool_executor=self._executor,
            sink=self._sink,
            context_manager=context_manager,
            inbox=self._inbox,
            current_model=lambda: self.model,
            current_provider=lambda: self.model_provider,
            current_tools=lambda: self.tools,
            stream=stream,
            set_working=self._set_working,
        )
        # 重试口径跟随当前 provider 的配置（构造与切换时同步；见 ReActLoop._config）
        self._loop._config = model_provider.config

        # ── 工具集 ──
        self._tools: dict[str, Tool] = self._bind_tools(tools or [])
        self._executor.set_tools(self._tools)
        self.context_manager.on_tools_changed(self.tools)

        # ── 配置 ──
        self._yolo: bool = yolo if yolo is not None else get_config().yolo
        self._cwd: Path | None = None
        self._loop.max_turns = max_turns

        # ── Interrupt hooks ──
        # hook_id → (label, hook)：label 只用于日志归因（如 "Bash pid=12345"），
        # interrupt 触发时与 pid 的对应关系直接可读，不用再从 history 反推。
        self._interrupt_hooks: dict[str, tuple[str, Callable[[], None]]] = {}

        # ── Worker 生命周期 ──
        self._working: bool = False
        # 当前 turn 的开始时刻（UTC）——working 状态期间有效，供 resume 的
        # SyncSessionEvent.turn_started_at 取值（前端据此恢复已耗时而非从
        # resume 时刻重算）。与 _working 同生命周期（_set_working 维护）。
        self._turn_started_at: datetime | None = None
        self._interrupt_lock = asyncio.Lock()
        # interrupt 可观测性（纯观测，不改变控制流；细节见 cancel_watch.py）：
        # 锁等待/持有超阈值告警、cancel 后 worker 不死看门狗、循环边界 cancelling 留痕。
        self._interrupt_lock_watch = InterruptLockWatch()
        self._cancel_watchdogs: set[asyncio.Task] = set()
        self._last_cancelling_traced = 0
        self._worker = asyncio.create_task(self._run())

    # ── ToolContext Protocol 实现 ──

    @property
    def session_id(self) -> str:
        return self.context_manager.id

    @property
    def media(self) -> MediaAccess | None:
        """会话媒体读写窄接口（工具经 ctx.media 写图；None = 无媒体存储）。"""
        return self._media

    @property
    def capabilities(self) -> ModelCapabilities:
        """当前模型的能力声明（实时解析，跟随 /model 切换）。

        无缓存：模型名是 agent 状态（self.model），声明是 provider 配置事实
        （model_provider.config）——两者各自单所有者，交给纯函数合成即可。
        """
        return resolve_model_capabilities(self.model_provider.config, self.model)

    @property
    def model_display_name(self) -> str | None:
        """当前模型的展示名（配置声明的投影；未声明 = None）。

        与 capabilities 同款实时解析：展示名不是身份，任何匹配 / 变更仍以
        model + provider 为准；前端只在展示层消费（缺省回落 self.model）。
        """
        return resolve_model_display_name(self.model_provider.config, self.model)

    @property
    def yolo(self) -> bool:
        return self._yolo

    @property
    def cwd(self) -> Path | None:
        return self._cwd

    def set_yolo(self, value: bool) -> None:
        self._yolo = value

    def set_cwd(self, path: Path | None) -> None:
        self._cwd = path

    async def ask_feedback(self, event: AskEvent, timeout: float) -> str:
        """Emit Ask 事件并等待用户按 tool_call_id 定向回复。"""
        tool_call_id = _current_tool_call_id.get() or uuid.uuid4().hex
        event.tool_call_id = tool_call_id
        if event.session_id is None:
            event.session_id = self.session_id
        self.emit(event)

        future = self._inbox.register_waiter(tool_call_id)
        try:
            return await asyncio.wait_for(future, timeout=timeout)
        finally:
            self._inbox.unregister_waiter(tool_call_id)

    def emit(self, event: WingEvent) -> None:
        """通过 EventBus 广播事件（工具侧 emit 入口）。

        路由到 sink 的统一分流路径：persist=true 即时落盘进链，
        persist=false 纯广播（不落盘、不缓冲）——与 react loop 事件同一出口，
        不存在绕过 sink 的直连 event_bus。
        """
        self._sink.emit(event)

    def register_interrupt_hook(self, hook: Callable[[], None], label: str = "") -> str:
        """注册 interrupt hook；``label`` 为日志归因说明（如 "Bash pid=12345"）。"""
        hook_id = uuid.uuid4().hex
        self._interrupt_hooks[hook_id] = (label, hook)
        return hook_id

    def unregister_interrupt_hook(self, hook_id: str) -> None:
        self._interrupt_hooks.pop(hook_id, None)

    # ── 对外接口 ──

    @property
    def has_pending_input(self) -> bool:
        """inbox 里是否有待处理输入（已投递、worker 尚未取走）。"""
        return self._inbox.has_pending

    @property
    def tools(self) -> list[Tool]:
        """当前可执行工具集（排序快照）。"""
        return sorted(self._tools.values(), key=lambda t: t.effective_llm_name)

    @property
    def sink(self) -> AgentEventSink:
        """事件发射出口（供 runtime / 工具侧发射事件）。"""
        return self._sink

    @property
    def turn_started_at(self) -> datetime | None:
        """当前 turn 的开始时刻（UTC）；无进行中 turn 时为 None。"""
        return self._turn_started_at

    def uncommitted_message(self) -> dict | None:
        """未提交的 assistant Message 投影（单个 Message 形状 dict | None）。

        按需快照当前一轮的 provider accumulator（未提交内容的唯一权威），
        取**已终结**块组装为 assistant Message 再投影为前端重放形状——与
        中断补提交同源（同一个 snapshot_blocks）。无进行中轮次或无已终结
        块时返回 None。MUST NOT 缓存副本。
        """
        from wing.session import serialize_message

        acc = self._loop.current_acc
        if acc is None:
            return None
        blocks = self.model_provider.snapshot_blocks(acc)
        if not blocks:
            return None
        return serialize_message(Message(role="assistant", content_blocks=blocks))

    def uncommitted_tools(self) -> list[dict]:
        """未终结 tool 调用的原始 args 片段列表（活工具卡渲染素材）。

        按需快照当前 accumulator 的 pending 投影——每项
        `{tool_call_id, tool_name, args_fragment}`，`args_fragment` 是原始
        参数文本（后端不解析，前端经既有局部解析路径渲染）。
        """
        acc = self._loop.current_acc
        if acc is None:
            return []
        return [
            {
                "tool_call_id": v.tool_call_id,
                "tool_name": v.tool_name,
                "args_fragment": v.args_fragment,
            }
            for v in self.model_provider.pending_tool_calls(acc)
        ]

    def pending_ask_ids(self) -> set[str]:
        """仍然挂起的 ask 的 tool_call_id 集合（源自 inbox feedback waiters）。

        resume 重放据此过滤链上的 AskEvent——只下发仍挂起的提问（已答/已
        失效的 ask 重放会渲染出活的 Ask 卡，用户回答进虚空）。
        """
        return self._inbox.pending_ask_ids()

    @property
    def max_turns(self) -> int | None:
        return self._loop.max_turns

    @property
    def status(self) -> Any:  # SessionStatus
        """Session 运行时状态。优先级：waiting > working > idle。"""
        if self._inbox.has_waiters:
            return "waiting"
        if self._working:
            return "working"
        return "idle"

    def set_tools(self, tool_names: list[str]) -> None:
        """设置工具集——唯一变更入口。"""
        from wing.tool_registry import tool_registry

        unbound_tools: list[Tool] = []
        for name in tool_names:
            tool = tool_registry.resolve(name)
            if tool is None:
                raise ValueError(f"cannot resolve tool reference: '{name}'")
            unbound_tools.append(tool)

        self._tools = self._bind_tools(unbound_tools)
        self._executor.set_tools(self._tools)
        self.context_manager.on_tools_changed(self.tools)
        log.info(f"set_tools: {len(self._tools)} tools")

    def set_max_turns(self, max_turns: int | None) -> None:
        self._loop.max_turns = max_turns

    def set_model(self, model: str, provider: ModelProvider) -> None:
        """切换模型与 provider（两参必填）。

        model 只由 WingAgent 持有（唯一存储）；provider 入表并换为活跃。
        ReActLoop / ToolExecutor 不存储 model——经注入取值器与调用点传参
        获取，无需传播。同 provider 内换 model 的调用方传当前 provider
        实例——任何 OpenAI-compat 端点都能给出 provider，可选 + fallback
        只引入隐式约定。
        """
        self.model = model
        self.model_provider = provider
        self._providers[provider.name] = provider
        self._loop._config = provider.config  # 重试口径跟随 provider 配置

    def get_or_create_provider(self, name: str) -> ModelProvider:
        """按 name 获取缓存的 provider client，缺失时创建并缓存（创建即拥有）。"""
        cached = self._providers.get(name)
        if cached is not None:
            return cached
        cfg = get_config().get_provider(name)
        provider = create_provider(cfg, session_id=self.session_id, media=self._media)
        self._providers[name] = provider
        return provider

    async def aclose_providers(self) -> None:
        """关闭并清空 provider client 表。

        仅由显式终结 provider 生命周期的一方调用：模板切换（旧 agent 整体
        废弃）、未来的 session 释放。shutdown() 不做此事——provider 的生命
        周期归创建 / 持有它的那一层（Session），agent 关停不关闭共享 client。
        """
        providers = list(self._providers.values())
        self._providers.clear()
        for provider in providers:
            await provider.aclose()

    async def rebuild_providers(self) -> None:
        """驱逐重建：按新配置重建活跃 provider，成功后关闭旧表全部 client。

        配置热加载入口——provider 客户端无"热刷新"语义（ModelProvider 不提供
        reload），reload 即驱逐 + 重建。api_key / base_url / anthropic_version /
        extra_body 等变更随重建自然生效；非活跃 name 下次用到时按新配置懒创建。

        先建后关：重建失败（如 provider 从新配置中移除）时旧 client 保持可用，
        session 不会被钉死在已关闭的 client 上。
        """
        active_name = self.model_provider.name
        cfg = get_config().get_provider(active_name)
        new_provider = create_provider(
            cfg, session_id=self.session_id, media=self._media
        )

        old_providers = list(self._providers.values())
        self._providers = {active_name: new_provider}
        self.model_provider = new_provider
        self._loop._config = new_provider.config  # 重试口径跟随 provider 配置
        for provider in old_providers:
            await provider.aclose()

    def set_reasoning_effort(self, effort: str | None) -> None:
        self.model_provider.reasoning_effort = effort

    async def post(
        self,
        content: str,
        request_id: str | None = None,
        role: str = "user",
        tool_call_id: str | None = None,
    ) -> None:
        """接收用户消息或 feedback。"""
        await self._inbox.post(content, request_id, role, tool_call_id)

    async def interrupt(self, request_id: str | None = None) -> None:
        """中断 Agent：触发 hooks、清理 inbox、等待旧 worker 补提交后重建。

        request_id 仅用于日志关联（网关端点生成并透传），不参与任何控制流。
        中断各阶段（entry / 锁 / cancel 快照 / worker 退休 / 总耗时）的日志
        与判读方式见 cancel_watch 模块文档。
        """
        tag = f"{self.session_id} req={(request_id or '-')[:8]}"
        started = time.monotonic()
        log.info(
            f"interrupt start [{tag}]: hooks={len(self._interrupt_hooks)} "
            f"lock_waiters={self._interrupt_lock_watch.waiters} "
            f"worker={task_label(self._worker)}"
        )

        # 触发所有 interrupt hooks（如杀子进程）
        self._fire_interrupt_hooks()

        # 取消 feedback waiters
        self._inbox.cancel_all_waiters()

        # 清空 inbox
        self._inbox.clear()

        # 取消旧 worker 并等待补提交
        async with self._interrupt_lock_watch.hold(self._interrupt_lock, tag):
            old = self._worker
            log_cancel_snapshot(old, context=tag)
            old.cancel()
            self._watch_cancelled_worker(old, tag)
            cancelled_at = time.monotonic()
            # await_result 只描述 `await old` 的出口（returned / cancelled /
            # error）——CancelledError 无法区分「old 被取消」与「本协程自己被
            # 取消」，老 worker 自身的终态由 cancelled= / done= 给出。
            await_result = "returned"
            try:
                await old
            except asyncio.CancelledError:
                await_result = "cancelled"
            except Exception:
                await_result = "error"
                log.exception(f"old worker died with error during interrupt [{tag}]")
            finally:
                self._worker = asyncio.create_task(self._run())
                log.info(
                    f"interrupt old worker retired [{tag}]: "
                    f"await_result={await_result} "
                    f"waited={int((time.monotonic() - cancelled_at) * 1000)}ms "
                    f"done={old.done()} cancelled={old.cancelled()} "
                    f"cancelling={old.cancelling()}"
                )
        log.info(
            f"Agent interrupted and reset [{tag}] "
            f"total={int((time.monotonic() - started) * 1000)}ms"
        )

    async def shutdown(self) -> None:
        """显式关闭 Agent：不重建 worker。

        注意：不关闭 provider——provider 生命周期由 Session 层管理，
        client 的终结由显式调用 aclose_providers() 的一方负责。
        """
        self._fire_interrupt_hooks()
        self._inbox.cancel_all_waiters()
        self._inbox.clear()

        self._worker.cancel()
        try:
            await self._worker
        except asyncio.CancelledError:
            pass
        log.info(f"Agent shutdown complete: session={self.session_id}")

    def get_status(self) -> dict:
        """返回当前状态快照，供 Session.initial_status 使用。"""
        count, tokens = self.context_manager.get_context_stats()
        ctx_window = 0
        if self.context_manager.compactor:
            ctx_window = self.context_manager.compactor.context_window_tokens
        return {
            "model": self.model,
            "thinking": self.model_provider.thinking,
            "reasoning_effort": self.model_provider.reasoning_effort,
            "message_count": count,
            "total_tokens": tokens,
            "context_window_tokens": ctx_window,
            "tools": [t.effective_llm_name for t in self.tools] if self.tools else [],
            "api_url": getattr(self.model_provider, "base_url", "unknown"),
        }

    # ── 内部 ──

    def _set_working(self, value: bool) -> None:
        self._working = value
        # turn_started_at 与 working 同生命周期：进入 working 登记开始时刻，
        # 退出（收口/中断/错误——run_turn 的 finally 必经此处）清空。
        if value:
            self._turn_started_at = datetime.now(timezone.utc)
        else:
            self._turn_started_at = None

    def _fire_interrupt_hooks(self) -> None:
        """触发 interrupt hooks（如杀子进程），逐条记录 label 以便归因。"""
        hooks = list(self._interrupt_hooks.items())
        labels = ", ".join(label or "(unlabeled)" for _, (label, _) in hooks) or "none"
        log.info(
            f"interrupt hooks [{self.session_id}]: firing {len(hooks)} hook(s): "
            f"{labels}"
        )
        for hook_id, (label, hook) in hooks:
            try:
                hook()
            except Exception as e:
                log.error(
                    f"interrupt hook {hook_id} ({label or 'unlabeled'}) failed: {e}"
                )

    def _watch_cancelled_worker(self, old: asyncio.Task, tag: str) -> None:
        """cancel 后复查旧 worker 是否真死（纯观测任务，不改变控制流）。"""
        watchdog = asyncio.create_task(
            watch_undead_task(old, context=tag, extra=self._worker_diag),
            name=f"cancel-watchdog:{tag}",
        )
        self._cancel_watchdogs.add(watchdog)
        watchdog.add_done_callback(self._cancel_watchdogs.discard)

    def _worker_diag(self) -> str:
        """看门狗 dump 的 agent 侧补充现场。"""
        return (
            f"agent: working={self._working} status={self.status} "
            f"pending_input={self._inbox.has_pending} "
            f"feedback_waiters={self._inbox.has_waiters} "
            f"new_worker={task_label(self._worker)}"
        )

    def _trace_cancelling(self) -> None:
        """worker 在每轮循环边界汇报 `cancelling()` 簿记。

        cancel「已投递但未死」时，下一轮迭代边界立刻留痕（INFO，含栈顶）；
        正常情况只有 DEBUG（文件日志始终落 DEBUG，取证时按 session 抓取）。
        """
        try:
            cancelling = self._worker.cancelling()
        except Exception:  # noqa: BLE001 - 观测不得外溢
            return
        if cancelling == self._last_cancelling_traced:
            log.debug(
                f"worker loop boundary [{self.session_id}]: cancelling={cancelling}"
            )
            return
        previous, self._last_cancelling_traced = (
            self._last_cancelling_traced,
            cancelling,
        )
        log.info(
            f"worker loop boundary [{self.session_id}]: cancelling "
            f"{previous} -> {cancelling} stack=[{stack_trace(self._worker)}]"
        )

    async def _run(self) -> None:
        """主循环：持续 drain inbox 并处理。"""
        while True:
            self._trace_cancelling()
            try:
                await self._loop.run_turn()
            except asyncio.CancelledError:
                raise
            except Exception as e:
                log.error(f"处理消息失败: {e}")
                self._sink.error(f"处理消息失败：异常：{e}")
                self._sink.done()

    def _bind_tools(self, tools: list[Tool]) -> dict[str, Tool]:
        from wing.tool_registry import ToolRef

        seen: dict[str, str] = {}
        for tool in tools:
            key = tool.effective_llm_name
            desc = str(ToolRef(namespace=tool.namespace, name=tool.name))
            if key in seen:
                raise ValueError(
                    f"LLM name collision: '{key}' is claimed by both "
                    f"{seen[key]} and {desc}"
                )
            seen[key] = desc

        bound_map: dict[str, Tool] = {}

        for tool in tools:
            if not tool.inject_agent_param:
                bound_map[tool.effective_llm_name] = tool
                continue

            original_fn = tool.function
            inject_name = tool.inject_agent_param

            async def wrapper(
                *args: Any,
                _agent: WingAgent = self,
                _fn: Callable = original_fn,
                _name: str = inject_name,  # type: ignore[assignment]
                **kwargs: Any,
            ) -> Any:
                kwargs[_name] = _agent
                return (
                    await _fn(*args, **kwargs)
                    if inspect.iscoroutinefunction(_fn)
                    else _fn(*args, **kwargs)
                )

            functools.update_wrapper(wrapper, original_fn)

            bound_tool = tool.model_copy(
                update={
                    "function": wrapper,
                    "inject_agent_param": None,
                }
            )
            bound_map[tool.effective_llm_name] = bound_tool

        return bound_map
