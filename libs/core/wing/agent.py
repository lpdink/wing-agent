# wing/agent.py

from __future__ import annotations

import asyncio
import functools
import inspect
import tempfile
import time
import uuid
from contextvars import ContextVar
from dataclasses import dataclass, field
from collections.abc import Callable
from pathlib import Path
from typing import Any

from wing.common.process import kill_process_group
from wing.event_bus import event_bus
from wing.event import EventTarget
from wing.request_context import reset_request_context, set_request_context
from wing.openai_provider import OpenAIProvider

from .agent_state_bag import AgentStateBag
from .common.logger import log
from .config import get_config
from .context_manager import ContextManager
from .hook_registry import hooks
from .schema import ToolError
from .event import (
    AssistantTurnEvent,
    AskEvent,
    ContextStatsEvent,
    DoneEvent,
    ErrorEvent,
    LLMCallMetricsEvent,
    SessionStatus,
    WingEvent,
    ReasoningEvent,
    TextEvent,
    ToolCallEvent,
    ToolCallResultEvent,
    ToolCallStreamEvent,
    ToolResultTurnEvent,
    TurnResultEvent,
    TurnStartedEvent,
)
from .schema import LLMUsage, Message, Tool, ToolCall


# 当前正在执行的工具调用 id。exec_tool_calls 为每个工具任务设置（gather 各 task
# 的 context 相互隔离），ask_feedback() 据此把 AskEvent 与 feedback waiter 关联。
_current_tool_call_id: ContextVar[str | None] = ContextVar(
    "current_tool_call_id", default=None
)


def current_tool_call_id() -> str | None:
    """当前执行上下文的工具调用 id（不在工具执行中时为 None）。

    工具据此把派生事件（如 DiffContentEvent）关联回自身的 ToolCall，
    使前端在并发乱序场景下仍能把事件锚定到正确的 ToolCall cell。
    """
    return _current_tool_call_id.get()


@dataclass
class Inbound:
    """进入 agent 的请求，携带消息和上下文元数据。

    未来可扩展字段：source（来源前端标识）、user（多用户场景）等。
    request_id 由 SM.post() 传入，_worker 处理时设置到 contextvar，
    使该消息触发的所有事件都携带同一个 request_id（用于审计和 Promise resolve）。
    """

    message: Message
    request_id: str | None = None


@dataclass
class _TurnAccumulator:
    """Accumulates state across a single agent turn (one user message → final response).

    Used by _process_turn to build TurnResultEvent without
    polluting agent instance state. Created fresh per turn.
    """

    num_turns: int = 0
    last_text: str = ""
    input_tokens: int = 0
    output_tokens: int = 0
    cached_tokens: int = 0
    start_time: float = field(default_factory=time.time)

    def record_usage(self, usage: LLMUsage | None) -> None:
        if usage:
            self.input_tokens += usage.prompt_tokens
            self.output_tokens += usage.completion_tokens
            self.cached_tokens += usage.cached_tokens

    def record_text(self, text: str | None) -> None:
        if text:
            self.last_text = text

    def elapsed_ms(self) -> int:
        return int((time.time() - self.start_time) * 1000)

    def usage_dict(self) -> dict:
        return {
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cached_tokens": self.cached_tokens,
        }


class WingAgent:
    def __init__(
        self,
        model: str,
        model_provider: OpenAIProvider,
        context_manager: ContextManager,
        stream: bool = False,
        tools: list[Tool] | None = None,
        max_turns: int | None = None,
        yolo: bool | None = None,
    ) -> None:
        self.stream = stream
        self.model_provider = model_provider
        self.model = model
        self._tool_map: dict[str, Tool] = self._bind_tools(tools or [])
        self.context_manager = context_manager
        self.state = AgentStateBag()
        self._steer = get_config().steer
        self._max_turns = max_turns
        self._yolo: bool = yolo if yolo is not None else get_config().yolo
        self._inbox: asyncio.Queue[Inbound] = asyncio.Queue()
        # Feedback waiters：tool_call_id → Future，严格寻址。
        # 工具通过 ask_feedback() 注册 waiter，用户回复携带 tool_call_id 定向 resolve。
        # 非空即表示 agent 处于 waiting 状态（status property 据此推导）。
        self._feedback_waiters: dict[str, asyncio.Future[str]] = {}
        # 是否有 turn 正在进行（TurnStarted 置真，turn 收尾置假）。用于推导 session 状态。
        self._working: bool = False
        self._worker = asyncio.create_task(self._run())

    @property
    def tools(self) -> list[Tool]:
        return sorted(self._tool_map.values(), key=lambda item: item.effective_llm_name)

    @property
    def max_turns(self) -> int | None:
        """Agent loop 最大轮数限制。None 表示无限。"""
        return self._max_turns

    @property
    def session_id(self) -> str:
        return self.context_manager.id

    @property
    def status(self) -> SessionStatus:
        """Session 运行时状态（后端为唯一事实源）。

        优先级：waiting > working > idle。
        - waiting：有工具阻塞在 feedback waiter，等待用户反馈
        - working：有 turn 正在进行（TurnStarted 已 emit、Done 未至）
        - idle：已 resume 但无进行中的 turn
        （inactive 由 SessionManager 依据是否加载进内存判定，不在此处。）
        """
        if self._feedback_waiters:
            return "waiting"
        if self._working:
            return "working"
        return "idle"

    @property
    def yolo(self) -> bool:
        """YOLO 模式是否启用。"""
        return self._yolo

    def set_yolo(self, value: bool) -> None:
        """设置 yolo 模式（供 /yolo 魔术命令和 apply_agent_override 调用）。"""
        self._yolo = value

    # ── 运行时覆盖方法（供 Session.apply_agent_override 调用）──

    def replace_tools(self, tool_names: list[str]) -> None:
        """替换工具列表。从 tool_registry 获取未绑定工具并重新绑定。

        使用 tool_registry.resolve() 解析工具引用（支持裸名和 namespace.name），
        获取全新的未绑定 Tool 对象，避免闭包泄漏（旧 agent 的 _agent 引用）。
        """
        from wing.tool_registry import tool_registry

        unbound_tools = [
            t for name in tool_names if (t := tool_registry.resolve(name)) is not None
        ]
        self._tool_map = self._bind_tools(unbound_tools)

    def set_max_turns(self, max_turns: int | None) -> None:
        """设置 agent loop 最大轮数。None 表示无限。"""
        self._max_turns = max_turns

    def set_reasoning_effort(self, effort: str | None) -> None:
        """设置 reasoning effort 级别。"""
        self.model_provider.reasoning_effort = effort

    def _bind_tools(self, tools: list[Tool]) -> dict[str, Any]:
        from wing.tool_registry import ToolRef

        # 检测 effective_llm_name 碰撞——同一 agent 内 LLM 可见名必须唯一
        seen: dict[str, str] = {}  # llm_name → ToolRef 描述
        for tool in tools:
            key = tool.effective_llm_name
            desc = str(ToolRef(namespace=tool.namespace, name=tool.name))
            if key in seen:
                raise ValueError(
                    f"LLM name collision: '{key}' is claimed by both "
                    f"{seen[key]} and {desc}"
                )
            seen[key] = desc

        bound_map = {}

        # NOTE: WARNING — 闭包 _agent=self 捕获了当前 agent 实例。
        # 当从另一个 session 重建 agent 时，必须传入 tool_registry.get_tool() 拿到的
        # 原始（未绑定）Tool 对象，否则旧 agent 的闭包会泄漏到新 agent 中。
        # 参见 session.py fork/switch 中 tools=unbound_tools 的处理。
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

    async def _run(self) -> None:
        """主循环：持续 drain inbox 并处理"""
        while True:
            try:
                await self._process_turn()
            except asyncio.CancelledError:
                raise
            except Exception as e:
                await self._handle_message_error(e)

    async def _process_turn(self) -> None:
        """处理一个 turn：drain inbox 中所有待处理消息，合并后执行 ReAct loop。

        消费语义为 drain-and-merge——block 等待首条消息，再 non-blocking
        取出剩余消息，将所有 user content 拼接为一条消息注入 context。
        这与 Steer 的 mid-loop drain 共享同一个 _drain_inbox() 原语。

        NOTE: batch 内非首条消息的 request_id 被有意丢弃。当前无 consumer
        依赖 per-request correlation（TUI 为纯事件驱动，HTTP send 入队即 ack）。
        若未来需要 request-level completion tracking，可在 merge 时为每条
        被合并消息 emit MergedEvent(request_id=...) 通知 transport 层。
        """
        first = await self._inbox.get()
        batch = [first] + self._drain_inbox()

        # Merge: 所有 user 消息内容拼接为一条 user message
        content = "\n".join(
            b.message.content
            for b in batch
            if b.message.role == "user" and b.message.content
        )
        if not content:
            return

        inbound = Inbound(
            message=Message(role="user", content=content),
            request_id=first.request_id,
        )

        # 设置 request_id 到当前协程 context
        # 该消息触发的所有事件（TextEvent, ToolCallEvent, DoneEvent 等）
        # 都会 auto-inject 此 request_id
        token = set_request_context(
            request_id=inbound.request_id, session_id=self.session_id
        )
        ctx = _TurnAccumulator()
        try:
            # Signal turn start — frontend uses this to show working indicator.
            self.emit(TurnStartedEvent(session_id=self.session_id))
            # 进入 working 状态（status property 据此推导），finally 中复位。
            self._working = True

            # Hook: before_user_message — 修改用户消息内容
            modified_content = await hooks.invoke_async(
                "before_user_message", inbound.message.content
            )
            if modified_content is not None:
                inbound.message.content = modified_content

            self.context_manager.add_message(inbound.message)

            while True:
                # Check max_turns limit before each LLM call
                if self._max_turns is not None and ctx.num_turns >= self._max_turns:
                    self.emit(
                        TurnResultEvent(
                            session_id=self.session_id,
                            subtype="error_max_turns",
                            is_error=True,
                            num_turns=ctx.num_turns,
                            duration_ms=ctx.elapsed_ms(),
                            usage=ctx.usage_dict(),
                            errors=[f"Reached max turns limit: {self._max_turns}"],
                        )
                    )
                    self.emit(DoneEvent(session_id=self.session_id))
                    return

                log.info(f"run with {inbound.message}, turn {ctx.num_turns}")
                if not await self._llm_turn(ctx):
                    ctx.num_turns += 1
                    break
                ctx.num_turns += 1

            # Success — emit TurnResultEvent then DoneEvent
            self.emit(
                TurnResultEvent(
                    session_id=self.session_id,
                    subtype="success",
                    result=ctx.last_text,
                    num_turns=ctx.num_turns,
                    duration_ms=ctx.elapsed_ms(),
                    usage=ctx.usage_dict(),
                )
            )
            self.emit(DoneEvent(session_id=self.session_id))
        except Exception as e:
            from wing.common.utils import format_exception_chain

            error_detail = format_exception_chain(e)
            log.exception(f"处理消息失败: {error_detail}")
            self.emit(
                TurnResultEvent(
                    session_id=self.session_id,
                    subtype="error_during_execution",
                    is_error=True,
                    num_turns=ctx.num_turns,
                    duration_ms=ctx.elapsed_ms(),
                    usage=ctx.usage_dict(),
                    errors=[error_detail],
                )
            )
            self.emit(
                ErrorEvent(
                    session_id=self.session_id,
                    message=f"处理消息失败：异常：{error_detail}",
                )
            )
            self.emit(DoneEvent(session_id=self.session_id))
        finally:
            # 退出 working 状态（覆盖成功/异常/max_turns/取消所有路径）。
            self._working = False
            reset_request_context(token)

    async def _llm_turn(self, ctx: _TurnAccumulator) -> bool:
        """
        执行一轮 LLM 生成 + 工具执行
        Returns: True 需要继续下一轮，False 对话结束
        """
        assistant_msg, pending_tool_calls = await self._call_llm()

        # ── Emit turn-level AssistantTurnEvent ──
        content_blocks: list[dict] = []
        if assistant_msg.reasoning_content:
            content_blocks.append(
                {"type": "thinking", "thinking": assistant_msg.reasoning_content}
            )
        if assistant_msg.content:
            content_blocks.append({"type": "text", "text": assistant_msg.content})
        for tc in pending_tool_calls:
            content_blocks.append(
                {
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.name,
                    "input": tc.arguments,
                }
            )

        usage_dict = None
        if assistant_msg.usage:
            usage_dict = {
                "input_tokens": assistant_msg.usage.prompt_tokens,
                "output_tokens": assistant_msg.usage.completion_tokens,
                "cached_tokens": assistant_msg.usage.cached_tokens,
            }

        self.emit(
            AssistantTurnEvent(
                session_id=self.session_id,
                content_blocks=content_blocks,
                model=self.model,
                stop_reason="tool_use" if pending_tool_calls else "end_turn",
                usage=usage_dict,
            )
        )

        # Update accumulator for TurnResultEvent
        ctx.record_usage(assistant_msg.usage)
        ctx.record_text(assistant_msg.content)

        tc_results = await self.exec_tool_calls(pending_tool_calls)

        # Steer: 开启时，drain inbox 中积攒的用户消息，附带到最后一个 tool result 开头
        if self._steer and tc_results:
            steer_notes = self._drain_inbox_for_steer()
            if steer_notes:
                tc_results[-1].content = steer_notes + (tc_results[-1].content or "")

        # 本轮产生的消息：assistant 响应 + 其触发的 tool 执行结果
        turn_messages = [assistant_msg] + tc_results
        self.context_manager.add_messages(turn_messages)

        # 每次 turn 后 emit context stats——前端据此更新状态栏
        # compact 发生在 get_messages_for_llm() 里，下次 _call_llm 调用时才执行
        # 所以 compact 后第一次 _llm_turn 的 stats 反映的是 compact 后的真实大小
        count, tokens = self.context_manager.get_context_stats()
        ctx_window = 0
        if self.context_manager.compactor:
            ctx_window = self.context_manager.compactor.context_window_tokens
        self.emit(
            ContextStatsEvent(
                session_id=self.session_id,
                message_count=count,
                total_tokens=tokens,
                context_window_tokens=ctx_window,
            )
        )

        if not pending_tool_calls:
            if not get_config().preserved_thinking:
                self.context_manager.clear_reasoning()
            return False
        return True

    def _drain_inbox(self) -> list[Inbound]:
        """Non-blocking drain: 取出 inbox 中所有待处理消息。

        唯一的 inbox drain 原语——入口 batch 合并和 Steer mid-loop 注入共用。
        """
        items: list[Inbound] = []
        while not self._inbox.empty():
            try:
                items.append(self._inbox.get_nowait())
            except asyncio.QueueEmpty:
                break
        return items

    def _drain_inbox_for_steer(self) -> str:
        """Drain pending inbox messages and format as steer notes.

        Returns a formatted string prepended to the last tool result,
        clearly marked as user steer guidance so the model can distinguish
        it from tool output.
        """
        items = self._drain_inbox()
        notes = [
            b.message.content
            for b in items
            if b.message.role == "user" and b.message.content
        ]
        if not notes:
            return ""
        return f"[User steer note: {'\n'.join(notes)}]\n"

    async def _call_llm(self) -> tuple[Message, list[ToolCall]]:
        """得到模型的响应和工具调用"""
        content_chunks: list[str] = []
        reasoning_chunks: list[str] = []
        pending_tool_calls: list[ToolCall] = []
        last_usage: LLMUsage | None = None

        async for chunk in self.model_provider.generate(
            messages=await self.context_manager.get_messages_for_llm(
                model=self.model,
                model_provider=self.model_provider,
                tools=self.tools,
            ),
            model=self.model,
            tools=self.tools,
            stream=self.stream,
        ):
            if chunk.reasoning_content:
                reasoning_chunks.append(chunk.reasoning_content)
                self.emit(
                    ReasoningEvent(
                        session_id=self.session_id, content=chunk.reasoning_content
                    )
                )

            if chunk.content:
                content_chunks.append(chunk.content)
                self.emit(TextEvent(session_id=self.session_id, content=chunk.content))

            if chunk.tool_calls:
                pending_tool_calls.extend(chunk.tool_calls)

            if chunk.tool_call_deltas:
                for delta in chunk.tool_call_deltas:
                    self.emit(
                        ToolCallStreamEvent(
                            session_id=self.session_id,
                            tool_call_id=delta.id,
                            tool_name=delta.name,
                            args_fragment=delta.args_fragment,
                            is_final=delta.is_final,
                        )
                    )

            if chunk.usage.completion_tokens or chunk.usage.prompt_tokens:
                last_usage = chunk.usage
                self.emit(
                    LLMCallMetricsEvent(
                        session_id=self.session_id,
                        prompt_tokens=chunk.usage.prompt_tokens,
                        completion_tokens=chunk.usage.completion_tokens,
                        cached_tokens=chunk.usage.cached_tokens,
                        first_chunk_rt_ms=chunk.usage.first_chunk_rt_ms,
                        tokens_per_sec=chunk.usage.tokens_per_sec,
                        model=chunk.usage.model,
                    )
                )

        assistant_msg = Message(
            role="assistant",
            content="".join(content_chunks),
            reasoning_content="".join(reasoning_chunks),
            tool_calls=pending_tool_calls,
            usage=last_usage,
        )

        return assistant_msg, pending_tool_calls

    async def exec_tool_calls(
        self, pending_tool_calls: list[ToolCall]
    ) -> list[Message]:
        log.info(f"exec_tool_calls: executing {len(pending_tool_calls)} tool calls")

        async def _exec_one(tc: ToolCall) -> Message:
            log.info(f"exec_tool_calls: executing tool '{tc.name}' with id={tc.id}")
            # 设置当前工具调用 id（gather 各 task context 独立，互不污染），
            # 供 ask_feedback() 关联 AskEvent 与 feedback waiter。
            token = _current_tool_call_id.set(tc.id)
            try:
                result = await self._execute_tool(tc)
                result = self._maybe_truncate(result)
            except Exception as e:
                # 安全兜底：_execute_tool 内部已捕获绝大多数异常，
                # 此处防止意外逃逸导致 asyncio.gather 中断其他并发任务
                log.error(f"exec_tool_calls: unexpected error in tool '{tc.name}': {e}")
                result = f"Error executing tool '{tc.name}': {e}"
            finally:
                _current_tool_call_id.reset(token)
            return Message(
                role="tool",
                tool_call_id=tc.id,
                content=result,
            )

        # asyncio.gather 并发执行所有工具调用，返回值顺序与输入顺序一致
        tc_results = list(
            await asyncio.gather(*(_exec_one(tc) for tc in pending_tool_calls))
        )
        return tc_results

    def _maybe_truncate(self, result: str) -> str:
        """Truncate tool result if it exceeds configured threshold.

        Full result is saved to a temp file so the agent can read it
        back via the Read tool if needed.
        """
        from wing.config import get_config, get_wing_home

        cfg = get_config().tool_result_truncate
        if cfg.max_length is None or cfg.max_length < 0:
            return result
        if len(result) <= cfg.max_length:
            return result

        # Clamp keep_chars to avoid head/tail overlap
        keep = min(cfg.keep_chars, len(result) // 2)

        # Save full result to persistent temp file
        try:
            tmp_dir = get_wing_home() / "tmp"
            tmp_dir.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="w",
                suffix=".txt",
                prefix="wing_truncated_",
                dir=str(tmp_dir),
                delete=False,
                encoding="utf-8",
            ) as f:
                f.write(result)
                full_path = Path(f.name)
            file_note = f"full result saved to: {full_path}. Use Read tool to view it."
        except OSError:
            log.warning("Failed to save truncated tool result to temp file")
            file_note = "full result could not be saved to disk."

        head = result[:keep]
        tail = result[-keep:] if keep > 0 else ""
        marker = f"... [truncated, original length: {len(result)} chars, {file_note}]"
        return f"{head}\n{marker}\n{tail}"

    async def _handle_message_error(self, error: Exception) -> None:
        """处理消息处理异常：通知"""
        log.error(f"处理消息失败: {error}")
        self.emit(
            ErrorEvent(
                session_id=self.session_id,
                message=f"处理消息失败：异常：{error}",
            )
        )
        self.emit(DoneEvent(session_id=self.session_id))

    async def _execute_tool(self, tc: ToolCall) -> str:
        # 发送工具调用开始消息
        self.emit(
            ToolCallEvent(
                session_id=self.session_id,
                tool_name=tc.name,
                tool_args=tc.arguments,
                tool_call_id=tc.id,
            )
        )

        tool = self._tool_map.get(tc.name)
        if not tool:
            result = f"Error: unknown tool: {tc.name}, or you don't have permission to use it."
            self.emit(
                ToolCallResultEvent(
                    session_id=self.session_id,
                    tool_name=tc.name,
                    tool_args=tc.arguments,
                    tool_call_id=tc.id,
                    tool_result=result,
                    tool_success=False,
                    model=self.model,
                )
            )
            self.emit(
                ToolResultTurnEvent(
                    session_id=self.session_id,
                    tool_use_id=tc.id,
                    tool_name=tc.name,
                    content=result,
                    is_error=True,
                )
            )
            return result

        try:
            # Hook: before_tool_call — 修改工具调用参数
            modified_tc = await hooks.invoke_async("before_tool_call", tc)
            if modified_tc is not None:
                tc = modified_tc

            # 过滤掉工具函数实际不接受的参数（如动态添加的 purpose）。
            # 远程工具的可执行体是 **kwargs 闭包（VAR_KEYWORD），其参数集合由
            # 远程 schema 完整定义，应全量透传——检测到 VAR_KEYWORD 时跳过过滤。
            sig = inspect.signature(tool.function)
            has_var_keyword = any(
                p.kind is inspect.Parameter.VAR_KEYWORD for p in sig.parameters.values()
            )
            if has_var_keyword:
                call_args = dict(tc.arguments)
            else:
                actual_params = set(sig.parameters.keys())
                call_args = {
                    k: v for k, v in tc.arguments.items() if k in actual_params
                }

            result = (
                await tool.function(**call_args)
                if inspect.iscoroutinefunction(tool.function)
                else tool.function(**call_args)
            )
            result_str = str(result)
            # Hook: after_tool_call — 修改工具调用结果
            # context 传递 tc 信息，handler 可据此过滤（如只截断 Bash result）
            modified_result = await hooks.invoke_async(
                "after_tool_call",
                result_str,
                tool_name=tc.name,
                tool_args=tc.arguments,
                tool_call_id=tc.id,
            )
            if modified_result is not None:
                result_str = modified_result
            # 发送工具调用结果消息
            self.emit(
                ToolCallResultEvent(
                    session_id=self.session_id,
                    tool_name=tc.name,
                    tool_args=tc.arguments,
                    tool_call_id=tc.id,
                    tool_result=result_str,
                    tool_success=True,
                    model=self.model,
                )
            )
            self.emit(
                ToolResultTurnEvent(
                    session_id=self.session_id,
                    tool_use_id=tc.id,
                    tool_name=tc.name,
                    content=result_str,
                    is_error=False,
                )
            )
            return result_str
        except ToolError as e:
            result = str(e)
            self.emit(
                ToolCallResultEvent(
                    session_id=self.session_id,
                    tool_name=tc.name,
                    tool_args=tc.arguments,
                    tool_call_id=tc.id,
                    tool_result=result,
                    tool_success=False,
                    model=self.model,
                )
            )
            self.emit(
                ToolResultTurnEvent(
                    session_id=self.session_id,
                    tool_use_id=tc.id,
                    tool_name=tc.name,
                    content=result,
                    is_error=True,
                )
            )
            return result
        except Exception as e:
            result = f"Error executing tool '{tc.name}': {e}"
            self.emit(
                ToolCallResultEvent(
                    session_id=self.session_id,
                    tool_name=tc.name,
                    tool_args=tc.arguments,
                    tool_call_id=tc.id,
                    tool_result=result,
                    tool_success=False,
                    model=self.model,
                )
            )
            self.emit(
                ToolResultTurnEvent(
                    session_id=self.session_id,
                    tool_use_id=tc.id,
                    tool_name=tc.name,
                    content=result,
                    is_error=True,
                )
            )
            return result

    async def ask_feedback(self, event: AskEvent, timeout: float) -> str:
        """Emit 一个 Ask 事件并等待用户按 tool_call_id 定向回复。

        工具侧等待反馈的唯一入口：
        - 从工具执行 context 读取当前 tool_call_id（exec_tool_calls 设置），
          缺失时（如测试中直接调用工具）退化为随机 id；
        - 将 id 注入 AskEvent 后 emit，客户端回复时回传该 id；
        - 注册 Future 到 _feedback_waiters，用户回复经 post() 按 id resolve。

        同一工具循环内重复调用（如 Bash 危险命令 re-ask）即以同 id 重新注册。
        超时抛 asyncio.TimeoutError，由调用方转换为 ToolError。
        """
        tool_call_id = _current_tool_call_id.get() or uuid.uuid4().hex
        event.tool_call_id = tool_call_id
        if event.session_id is None:
            event.session_id = self.session_id
        self.emit(event)

        future: asyncio.Future[str] = asyncio.get_running_loop().create_future()
        self._feedback_waiters[tool_call_id] = future
        try:
            return await asyncio.wait_for(future, timeout=timeout)
        finally:
            self._feedback_waiters.pop(tool_call_id, None)

    def interrupt(self) -> None:
        """中断 Agent：终止子进程、清理 inbox、重置事件循环"""

        # 终止正在执行的子进程（如 Bash 命令）
        # start_new_session=True 使子进程在独立 process group 中，
        # 必须用 killpg 杀整棵树，否则 cmd & 产生的孤儿进程会继续运行
        process = self.state.get("_active_process")
        if process is not None:
            kill_process_group(process)
            log.info("Active process group killed on interrupt")
            self.state.delete("_active_process")

        # 取消所有 feedback waiter（工具可能在等待用户反馈，打断后无人应答）
        self._cancel_feedback_waiters()

        # 清理 inbox 中的所有消息
        while not self._inbox.empty():
            try:
                self._inbox.get_nowait()
            except asyncio.QueueEmpty:
                break

        # 停止当前 worker 并创建新的事件循环
        self._worker.cancel()
        self._worker = asyncio.create_task(self._run())
        log.info("Agent interrupted and reset")

    def _cancel_feedback_waiters(self) -> None:
        """取消并清空所有 feedback waiter。

        interrupt/shutdown 时调用：等待中的工具随 worker 取消而终止，
        Future 显式 cancel 避免悬挂与告警。
        """
        for future in self._feedback_waiters.values():
            future.cancel()
        self._feedback_waiters.clear()

    async def shutdown(self) -> None:
        """显式关闭 Agent：清空 inbox，cancel worker 并等待其完成。

        用于 switch_template 场景下干净地关闭旧 agent。
        与 interrupt() 不同：shutdown 不重建 worker。
        """
        # 终止正在执行的子进程
        process = self.state.get("_active_process")
        if process is not None:
            kill_process_group(process)
            self.state.delete("_active_process")

        # 取消 feedback waiter
        self._cancel_feedback_waiters()

        # 清空 inbox
        while not self._inbox.empty():
            try:
                self._inbox.get_nowait()
            except asyncio.QueueEmpty:
                break

        self._worker.cancel()
        try:
            await self._worker
        except asyncio.CancelledError:
            pass
        log.info(f"Agent shutdown complete: session={self.session_id}")

    async def post(
        self,
        content: str,
        request_id: str | None = None,
        role: str = "user",
        tool_call_id: str | None = None,
    ) -> None:
        """接收用户消息或 feedback。

        request_id 由 SM.post() 传入，用于审计追踪和 SDK Promise resolve。

        Feedback 严格寻址：只有携带 tool_call_id 且命中 waiter 的消息才会
        resolve 对应 Future；无 id（普通用户消息、Explorer 等内部通知）
        或 id 已失效（waiter 超时）的消息一律进 inbox 作为新用户消息。
        """
        if tool_call_id is not None:
            future = self._feedback_waiters.get(tool_call_id)
            if future is not None and not future.done():
                log.info(
                    f"agent.post: resolving feedback waiter "
                    f"tool_call_id={tool_call_id} with '{content[:40]}'"
                )
                future.set_result(content)
                return
            log.info(
                f"agent.post: no live feedback waiter for tool_call_id="
                f"{tool_call_id}, falling through to inbox"
            )

        msg = Message(role=role, content=content)  # ty: ignore
        await self._inbox.put(Inbound(message=msg, request_id=request_id))
        log.info("post done")

    def emit(self, event: WingEvent) -> None:
        """通过 EventBus 广播事件。同步调用，不阻塞 agent。

        如果事件没有设置 target，自动设为 scope="session"。
        agent 的事件几乎总是 session 级别的。
        """
        if event.target is None:
            event.target = EventTarget(scope="session")
        event_bus.emit(event)

    def get_status(self) -> dict:
        """返回当前状态快照，供 Session.initial_status 使用."""
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
