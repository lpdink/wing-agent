# wing/agent/react_loop.py
"""ReActLoop — 主循环编排。

职责单一：drain inbox → hook → add message → while loop（LLM → tools →
steer → commit）→ emit TurnResult/Done。不持有工具执行、LLM 调用、
事件发射的实现细节——全部委托给注入的协作者。
"""

from __future__ import annotations

import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.config import get_config
from wing.hook_registry import hooks
from wing.request_context import reset_request_context, set_request_context
from wing.schema import LLMUsage, Message

from .event_sink import AgentEventSink
from .inbox import Inbox
from .llm_caller import LLMCaller
from .tool_executor import InterruptedToolResults, ToolExecutor

if TYPE_CHECKING:
    from wing.context_manager import ContextManager
    from wing.provider.base import ModelProvider
    from wing.schema import Tool


@dataclass
class _TurnAccumulator:
    """单次 turn 的累计状态（局部对象，不暴露）。"""

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


class ReActLoop:
    """ReAct 主循环编排器。"""

    def __init__(
        self,
        llm_caller: LLMCaller,
        tool_executor: ToolExecutor,
        sink: AgentEventSink,
        context_manager: ContextManager,
        inbox: Inbox,
        model: str,
        model_provider: ModelProvider,
        current_tools: Callable[[], list[Tool]],
        stream: bool = True,
        set_working: Callable[[bool], None] | None = None,
    ) -> None:
        self._llm = llm_caller
        self._executor = tool_executor
        self._sink = sink
        self._cm = context_manager
        self._inbox = inbox
        self._model = model
        self._model_provider = model_provider
        self._current_tools = current_tools
        self._stream = stream
        self._set_working = set_working
        # 运行时可变配置（由 WingAgent 设置）
        self.max_turns: int | None = None
        self.steer: bool = get_config().steer

    def set_model(self, model: str, provider: "ModelProvider | None" = None) -> None:
        """切换模型（及可选的 provider）——显式接口，供 WingAgent 委托。"""
        self._model = model
        if provider is not None:
            self._model_provider = provider

    async def run_turn(self) -> None:
        """处理一个 turn：drain inbox → merge → ReAct loop → TurnResult/Done。

        消费语义为 drain-and-merge——block 等待首条消息，再 non-blocking
        取出剩余消息，将所有 user content 拼接为一条消息注入 context。
        """
        first = await self._inbox.get()
        batch = [first] + self._inbox.drain()

        content = "\n".join(
            b.message.content
            for b in batch
            if b.message.role == "user" and b.message.content
        )
        if not content:
            return

        message = Message(role="user", content=content)
        request_id = first.request_id

        # 进入 working 状态（inbox.get 返回后才设置——等待期间为 idle）
        if self._set_working:
            self._set_working(True)

        token = set_request_context(request_id=request_id, session_id=self._cm.id)
        ctx = _TurnAccumulator()
        try:
            self._sink.turn_started()

            # Hook: before_user_message
            modified_content = await hooks.invoke_async(
                "before_user_message", message.content
            )
            if modified_content is not None:
                message.content = modified_content

            self._cm.add_message(message)

            while True:
                if self.max_turns is not None and ctx.num_turns >= self.max_turns:
                    self._sink.turn_result(
                        subtype="error_max_turns",
                        is_error=True,
                        num_turns=ctx.num_turns,
                        duration_ms=ctx.elapsed_ms(),
                        usage=ctx.usage_dict(),
                        errors=[f"Reached max turns limit: {self.max_turns}"],
                    )
                    self._sink.done()
                    return

                log.info(f"run with {message}, turn {ctx.num_turns}")
                if not await self._llm_turn(ctx):
                    ctx.num_turns += 1
                    break
                ctx.num_turns += 1

            self._sink.turn_result(
                subtype="success",
                result=ctx.last_text,
                num_turns=ctx.num_turns,
                duration_ms=ctx.elapsed_ms(),
                usage=ctx.usage_dict(),
            )
            self._sink.done()
        except Exception as e:
            from wing.common.utils import format_exception_chain

            error_detail = format_exception_chain(e)
            log.exception(f"处理消息失败: {error_detail}")
            self._sink.turn_result(
                subtype="error_during_execution",
                is_error=True,
                num_turns=ctx.num_turns,
                duration_ms=ctx.elapsed_ms(),
                usage=ctx.usage_dict(),
                errors=[error_detail],
            )
            self._sink.error(f"处理消息失败：异常：{error_detail}")
            self._sink.done()
        finally:
            if self._set_working:
                self._set_working(False)
            reset_request_context(token)

    async def _llm_turn(self, ctx: _TurnAccumulator) -> bool:
        """执行一轮 LLM 生成 + 工具执行。

        Returns: True 需要继续下一轮，False 对话结束。
        """
        # LLM 调用
        llm_result = await self._cm.get_messages_for_llm(
            model=self._model,
            model_provider=self._model_provider,
            current_tools=self._current_tools,
        )
        assistant_msg = await self._llm.call(
            messages=llm_result.messages,
            model=self._model,
            tools=llm_result.tools,
            stream=self._stream,
        )

        # Emit turn-level AssistantTurnEvent
        self._sink.assistant_turn(assistant_msg, self._model)

        # Update accumulator
        ctx.record_usage(assistant_msg.usage)
        ctx.record_text(assistant_msg.content)

        # 工具执行
        pending_tool_calls = assistant_msg.tool_calls or []
        try:
            tc_results = await self._executor.execute(pending_tool_calls)
            interrupted_exc: InterruptedToolResults | None = None
        except InterruptedToolResults as e:
            tc_results = e.results
            interrupted_exc = e

        # Steer: drain inbox 中积攒的用户消息
        if self.steer and tc_results:
            steer_notes = self._inbox.drain_for_steer()
            if steer_notes:
                tc_results[-1].content = steer_notes + (tc_results[-1].content or "")

        # 提交本轮消息
        turn_messages = [assistant_msg] + tc_results
        self._cm.add_messages(turn_messages)

        # Emit context stats
        count, tokens = self._cm.get_context_stats()
        ctx_window = 0
        if self._cm.compactor:
            ctx_window = self._cm.compactor.context_window_tokens
        self._sink.context_stats(count, tokens, ctx_window)

        # 恢复中断
        if interrupted_exc is not None:
            raise interrupted_exc.original

        if not pending_tool_calls:
            if not get_config().preserved_thinking:
                self._cm.clear_reasoning()
            return False
        return True
