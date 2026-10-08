# wing/agent/core.py
"""WingAgent — 瘦壳：组装 + 对外接口 + worker 生命周期。

实现 ToolContext Protocol，供工具侧通过窄接口访问 agent 能力。
内部编排委托给 ReActLoop / ToolExecutor / AgentEventSink / Inbox。
"""

from __future__ import annotations

import asyncio
import functools
import gc
import inspect
import time
import types
import uuid
from collections import deque
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
from wing.event import AskEvent, NoticeEvent, WingEvent
from wing.provider import create_provider
from wing.provider.base import ModelProvider
from wing.schema import Message, Tool

from .event_sink import AgentEventSink
from .inbox import Inbox
from .react_loop import ReActLoop
from .tool_executor import ToolExecutor, _current_tool_call_id

if TYPE_CHECKING:
    from wing.media import MediaAccess


# ── 打断收口阶梯 ──
# worker 的取消可能被吸收（取消计数已增加但 CancelledError 未投递——asyncio
# 的已知形态；CPython `Task.cancel()` 注释明言 "we may have to cancel it again
# later"）。对可重投的等待（挂起的 Future/Task），第二次 cancel 会重新投递
# ——典型吸收形态可被重投击穿；对不可取消的等待（如线程池 future），重投
# 只能挂账到等待结束——这正是收口必须保持有界的原因。单次等待必须覆盖
# `tool_executor._INTERRUPT_GATHER_TIMEOUT`（工具收尸的 5s 兜底）再加提交
# 余量——两个常量联动：重投取消若落在收尸路径内，本轮 partial 提交会整体
# 丢失（链仍自洽）。最坏 `_INTERRUPT_WAIT_SECONDS × _INTERRUPT_MAX_CANCELS`
# （默认 ~18s）后放手：保留旧 worker、绝不重建第二个，并登记终局续期（排在
# 另一个 interrupt 之后的请求，总等待还要叠加前者的收口时间）。
_INTERRUPT_WAIT_SECONDS = 6.0
_INTERRUPT_MAX_CANCELS = 3

#: 无帧中转对象：唯一 awaitable 引用只挂在 GC referents 上（见 `_descend`）。
_REFERENT_WRAPPERS = frozenset(
    {"async_generator_asend", "async_generator_athrow", "FutureIter"}
)

#: GC referents 兜底只认这些类型，避免误抓无关引用。
_REFERENT_AWAITABLES: tuple[type, ...] = (
    asyncio.Future,
    types.CoroutineType,
    types.AsyncGeneratorType,
    types.GeneratorType,
)


def _worker_frames(worker: asyncio.Task[Any], *, limit: int = 8) -> str:
    """worker 挂起点的浅栈（最内层在左，`<-` 连接）。

    `Task.get_stack()` 在 3.12 只给顶层帧；这里沿 `cr_await` / `ag_await` /
    `gi_yieldfrom` 一路走到真正的暂停点，并穿透三类无帧中转对象：`await
    <Task>`（FutureIter）与 `async for`（asend）经 GC referents 找唯一
    awaitable，gather 的 `_GatheringFuture` 经 `_children` 取单一 child
    （多 child 只标注不猜）。超长链截断保留最内层。任何异常都退回
    `get_stack()` 或占位符——诊断不得外溢。
    """

    def _location(obj: Any) -> str:
        return (
            f"{Path(obj.f_code.co_filename).name}:{obj.f_lineno}:{obj.f_code.co_name}"
        )

    def _frame_of(obj: Any) -> Any:
        return (
            getattr(obj, "cr_frame", None)
            or getattr(obj, "ag_frame", None)
            or getattr(obj, "gi_frame", None)
        )

    def _descend(current: Any) -> tuple[Any, str | None]:
        """下钻一层：返回 (下一跳, 标记)；标记用于无法下钻的分叉点。"""
        nxt = (
            getattr(current, "cr_await", None)
            or getattr(current, "ag_await", None)
            or getattr(current, "gi_yieldfrom", None)
        )
        if nxt is not None:
            return nxt, None
        if isinstance(current, asyncio.Task):
            return current.get_coro(), None
        children = getattr(current, "_children", None)  # asyncio.gather 的 future
        if children is not None:
            kids = list(children)
            if len(kids) == 1:
                return kids[0], None
            return None, f"<gather ×{len(kids)}>"
        if type(current).__name__ in _REFERENT_WRAPPERS:
            candidates = [
                ref
                for ref in gc.get_referents(current)
                if isinstance(ref, _REFERENT_AWAITABLES)
            ]
            return (candidates[0], None) if len(candidates) == 1 else (None, None)
        return None, None

    try:
        # deque(maxlen)：超长链时保留**最内层**（真正的暂停点），而不是外层；
        # 被截掉的外层在输出右端以 `…` 标注（读日志的人要能分辨"停在那"与
        # "被截断"）。
        steps: deque[Any] = deque(maxlen=limit)
        visited = 0
        current: Any = worker.get_coro()
        seen: set[int] = set()
        while current is not None and id(current) not in seen:
            seen.add(id(current))
            frame = _frame_of(current)
            if frame is not None:
                steps.append(frame)
                visited += 1
            current, marker = _descend(current)
            if marker is not None:
                steps.append(marker)
                visited += 1
        if not steps:
            steps.extend(worker.get_stack()[-4:])
        text = " <- ".join(
            step if isinstance(step, str) else _location(step)
            for step in reversed(steps)
        )
        if visited > limit:
            text = f"{text} <- …"
        return text or "<none>"
    except Exception:  # noqa: BLE001 - 诊断不得外溢
        return "<unavailable>"


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
        必须据此安全拒绝（不得假定可用）。"""

        # Provider client 表：按 name 有界持有（切回同名复用、跨 provider 切模型
        # 不关闭旧 client，不打断在途生成）。生命周期由创建方终结：shutdown() 不动
        # provider，仅 aclose_providers()（agent 整体废弃 / session 释放）关闭。
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
        self._interrupt_hooks: dict[str, Callable[[], None]] = {}

        # ── Worker 生命周期 ──
        self._working: bool = False
        # 当前 turn 的开始时刻（UTC）——working 状态期间有效，供 resume 的
        # SyncSessionEvent.turn_started_at 取值（前端据此恢复已耗时而非从
        # resume 时刻重算）。与 _working 同生命周期（_set_working 维护）。
        self._turn_started_at: datetime | None = None
        self._interrupt_lock = asyncio.Lock()
        # shutdown 是一次性的终局：置位后终局续期（见 `_arm_worker_renewal`）
        # 与 interrupt 的直接重建都不再发生——"逐出后又被复活"必须不可能。
        self._closing: bool = False
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

    def register_interrupt_hook(self, hook: Callable[[], None]) -> str:
        """注册 interrupt hook（如杀子进程），返回注销用 id。"""
        hook_id = uuid.uuid4().hex
        self._interrupt_hooks[hook_id] = hook
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
        """设置工具集——唯一变更入口。

        经 registry 重新解析为未绑定工具（不沿用旧实例的绑定闭包）。
        """
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

    async def interrupt(self) -> list[str]:
        """中断 Agent：清理积压、触发 hooks、取消旧 worker 后重建。

        积压在**等锁之前**清（入口处同步执行）：打断时刻之前排队的输入视为
        放弃；推迟到拿锁之后再清会把锁等待期间新到的消息（客户端 POST 已
        返回 ok）一并吞掉——排队等待期在正常路径就有秒级，单次降级路径最长
        ~18s（排队在另一个 interrupt 之后还会叠加）。hooks 与收口在锁内：
        注定排队的请求不提前杀掉在途 turn 的前台工具，也不会并发重建出
        第二个消费者。

        收口等待是**有界**的取消阶梯（见 `_stop_worker`）：worker 在阶梯内
        始终不终止时**保留旧 worker**（绝不重建第二个，避免两个 worker 抢
        同一个 inbox），打 ERROR 并广播 notice 后立即返回；同时登记终局
        续期——被保留的 worker 随后若自然终止且仍是当前 worker，自动重建
        消费者（否则消息进 inbox 无人消费）。interrupt 绝不会因为 worker
        不响应而永久持有 `_interrupt_lock`。`_closing`（shutdown 的终局闸门）
        同样挡住这里的重建：已关闭的 agent 不会被 interrupt 复活。

        Returns:
            被放弃的积压输入的 `request_id` 列表（打断时刻已排队、尚未被
            消费的输入）。调用方（runtime）把它带进 `InterruptedEvent`——
            前端据此只把真正被丢弃的消息标为 discarded，不误伤锁等待期间
            新到的消息。

        成功路径有且至少一条 INFO（`Agent interrupt complete: …`，与
        `shutdown()` 的 `Agent shutdown complete: …` 对称）：session id、
        worker 是否终止、consumer 是否重建、入口到返回的总耗时（含等锁与
        取消阶梯）、被丢弃的积压数——打断过程本身必须可 grep，而不是只能
        从前端事件流 / history 反推。阶梯中间步骤仍是 WARNING 级、阶梯
        耗尽另有 ERROR + notice（既有形态，不在此重复）。
        """
        started = time.monotonic()
        self._inbox.cancel_all_waiters()
        dropped = [b.request_id for b in self._inbox.clear() if b.request_id]

        rebuilt = False
        async with self._interrupt_lock:
            self._fire_interrupt_hooks()

            old = self._worker
            try:
                stopped = await self._stop_worker(old)
            except asyncio.CancelledError:
                # 本协程自身被取消（无法与 worker 收口区分）——沿用既有语义：
                # 吞掉，并按 worker 的实际状态决定后续。
                stopped = old.done()
            if stopped:
                # 终局续期可能已抢先重建（worker 在等待期间正好死亡），
                # shutdown 也可能已置位 _closing——只在还是旧 worker 且未关闭
                # 时重建，避免出现第二个消费者 / 复活已关闭的 agent。
                if self._worker is old and not self._closing:
                    self._worker = asyncio.create_task(self._run())
                    rebuilt = True
            else:
                self._arm_worker_renewal(old)
        log.info(
            f"Agent interrupt complete: session={self.session_id} "
            f"worker_stopped={stopped} consumer_rebuilt={rebuilt} "
            f"waited={int((time.monotonic() - started) * 1000)}ms "
            f"dropped={len(dropped)}"
        )
        return dropped

    async def shutdown(self) -> None:
        """显式关闭 Agent：取消阶梯收口，不重建 worker。

        收口与 interrupt 共用同一阶梯（有界）：worker 不响应取消时打 ERROR
        后返回——会话拆解（逐出 / release / 模板切换）绝不会被拖死；`_closing`
        置位后终局续期也被闸门挡住（不会"逐出后又被复活"）。
        注意：不关闭 provider——provider 生命周期由 Session 层管理，
        client 的终结由显式调用 aclose_providers() 的一方负责。
        """
        self._closing = True
        # 清积压留在锁内无妨：shutdown 是终局，锁等待期间到达的输入注定无人
        # 消费（`_closing` 已挡住一切重建）——不存在 interrupt 的"吞消息"语义。
        async with self._interrupt_lock:
            self._fire_interrupt_hooks()
            self._inbox.cancel_all_waiters()
            self._inbox.clear()

            worker = self._worker
            try:
                stopped = await self._stop_worker(worker, notify=False)
            except asyncio.CancelledError:
                stopped = worker.done()
        log.info(
            f"Agent shutdown complete: session={self.session_id} "
            f"worker_stopped={stopped}"
        )

    async def _stop_worker(self, worker: asyncio.Task, *, notify: bool = True) -> bool:
        """cancel + 阶梯等待 worker 收口；返回是否已终止（等待保证有界）。

        阶梯：cancel → 最多等 `_INTERRUPT_WAIT_SECONDS` → 未死则重投取消
        （对可重投的等待，重投会重新投递，专治「只记数不投递」的吸收；不可
        取消的等待只能挂账——所以阶梯保持有界、耗尽即放手）→ 最多
        `_INTERRUPT_MAX_CANCELS` 次。耗尽仍未终止：保留 worker 原样
        （由调用方决定语义），ERROR 报告现场，`notify=True` 时广播 notice。
        """
        for attempt in range(1, _INTERRUPT_MAX_CANCELS + 1):
            if worker.done():
                break
            if attempt > 1:
                # 重投前重放 hooks：第一次取消被吞后 worker 仍活着，可能又起了
                # 新子进程——killpg 幂等，重放成本为零。
                self._fire_interrupt_hooks()
            worker.cancel()
            done, _ = await asyncio.wait({worker}, timeout=_INTERRUPT_WAIT_SECONDS)
            if done:
                break
            if attempt < _INTERRUPT_MAX_CANCELS:
                log.warning(
                    f"interrupt: worker 在 cancel #{attempt} 后 "
                    f"{_INTERRUPT_WAIT_SECONDS:g}s 未终止，重投取消 "
                    f"(attempt {attempt + 1}/{_INTERRUPT_MAX_CANCELS})"
                )
        if not worker.done():
            self._report_undead_worker(worker, notify=notify)
            return False
        if not worker.cancelled():
            exc = worker.exception()
            if exc is not None:
                log.error("old worker died with error during interrupt", exc_info=exc)
        return True

    def _arm_worker_renewal(self, worker: asyncio.Task) -> None:
        """阶梯放手后的终局续期：被保留的 worker 随后死亡则重建消费者。

        只在「仍是当前 worker 且 agent 未关闭（`_closing`）」时重建。续期
        与 interrupt 重建可能竞争 worker 的同一终局——两边都以
        `self._worker is worker` 为准绳（同步回调里串行判定），不会出现
        第二个消费者。
        """

        def _renew(_: asyncio.Task) -> None:
            if self._closing or self._worker is not worker:
                return
            log.warning(
                f"interrupt: 被保留的 worker 随后终止，重建消费者 "
                f"(session={self.session_id})"
            )
            self._worker = asyncio.create_task(self._run())

        worker.add_done_callback(_renew)

    def _report_undead_worker(self, worker: asyncio.Task, *, notify: bool) -> None:
        """阶梯耗尽：worker 不响应取消——保留它（不重建），ERROR + notice。"""
        fut_waiter = getattr(worker, "_fut_waiter", None)
        log.error(
            f"interrupt: worker 在 {_INTERRUPT_MAX_CANCELS} 次取消后仍未终止；"
            f"保留为当前 worker（不重建）：task={worker.get_name()} "
            f"cancelling={worker.cancelling()} "
            f"fut_waiter={type(fut_waiter).__name__ if fut_waiter is not None else '-'} "
            f"stack=[{_worker_frames(worker)}]"
        )
        if notify:
            self._sink.emit(
                NoticeEvent(
                    level="error",
                    message=(
                        "打断未生效：worker 未在取消阶梯内终止（保留原 worker）"
                        "——若其随后退出将自动恢复；也可再次打断重试"
                    ),
                )
            )

    def get_status(self) -> dict:
        """返回当前状态快照（网关的 session info 投影素材）。"""
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
        """触发 interrupt hooks（如杀子进程）。"""
        for hook_id, hook in list(self._interrupt_hooks.items()):
            try:
                hook()
            except Exception as e:
                log.error(f"interrupt hook {hook_id} failed: {e}")

    async def _run(self) -> None:
        """主循环：持续 drain inbox 并处理。"""
        while True:
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
