# wing/agent/core.py
"""WingAgent — 瘦壳：组装 + 对外接口 + worker 生命周期。

实现 ToolContext Protocol，供工具侧通过窄接口访问 agent 能力。
内部编排委托给 ReActLoop / ToolExecutor / LLMCaller / AgentEventSink / Inbox。
"""

from __future__ import annotations

import asyncio
import functools
import inspect
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import Any

from wing.common.logger import log
from wing.config import get_config
from wing.event import AskEvent, EventTarget, WingEvent
from wing.event_bus import event_bus
from wing.provider.base import ModelProvider
from wing.schema import Tool

from .event_sink import AgentEventSink
from .inbox import Inbox
from .llm_caller import LLMCaller
from .react_loop import ReActLoop
from .tool_executor import ToolExecutor, _current_tool_call_id


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
    ) -> None:
        self.model = model
        self.model_provider = model_provider
        self.context_manager = context_manager
        self.stream = stream

        # ── 内部部件组装 ──
        self._inbox = Inbox()
        self._sink = AgentEventSink(session_id=self.session_id)
        self._llm_caller = LLMCaller(model_provider, self._sink)
        self._executor = ToolExecutor(self._sink, model)
        self._loop = ReActLoop(
            llm_caller=self._llm_caller,
            tool_executor=self._executor,
            sink=self._sink,
            context_manager=context_manager,
            inbox=self._inbox,
            model=model,
            model_provider=model_provider,
            current_tools=lambda: self.tools,
            stream=stream,
            set_working=self._set_working,
        )

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
        self._interrupt_lock = asyncio.Lock()
        self._worker = asyncio.create_task(self._run())

    # ── ToolContext Protocol 实现 ──

    @property
    def session_id(self) -> str:
        return self.context_manager.id

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
        """通过 EventBus 广播事件（工具侧 emit 入口）。"""
        if event.target is None:
            event.target = EventTarget(scope="session")
        event_bus.emit(event)

    def register_interrupt_hook(self, hook: Callable[[], None]) -> str:
        hook_id = uuid.uuid4().hex
        self._interrupt_hooks[hook_id] = hook
        return hook_id

    def unregister_interrupt_hook(self, hook_id: str) -> None:
        self._interrupt_hooks.pop(hook_id, None)

    # ── 对外接口 ──

    @property
    def tools(self) -> list[Tool]:
        """当前可执行工具集（排序快照）。"""
        return sorted(self._tools.values(), key=lambda t: t.effective_llm_name)

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

    async def interrupt(self) -> None:
        """中断 Agent：触发 hooks、清理 inbox、等待旧 worker 补提交后重建。"""
        # 触发所有 interrupt hooks（如杀子进程）
        self._fire_interrupt_hooks()

        # 取消 feedback waiters
        self._inbox.cancel_all_waiters()

        # 清空 inbox
        self._inbox.clear()

        # 取消旧 worker 并等待补提交
        async with self._interrupt_lock:
            old = self._worker
            old.cancel()
            try:
                await old
            except asyncio.CancelledError:
                pass
            except Exception:
                log.exception("old worker died with error during interrupt")
            finally:
                self._worker = asyncio.create_task(self._run())
        log.info("Agent interrupted and reset")

    async def shutdown(self) -> None:
        """显式关闭 Agent：不重建 worker。"""
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

    def _fire_interrupt_hooks(self) -> None:
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

    def _bind_tools(self, tools: list[Tool]) -> dict[str, Any]:
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

        bound_map: dict[str, Any] = {}

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
