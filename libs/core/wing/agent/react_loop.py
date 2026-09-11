# wing/agent/react_loop.py
"""ReActLoop — 主循环编排。

职责单一：drain inbox → hook → add message → while loop（LLM → tools →
steer → commit）→ emit TurnResult/Done。不持有工具执行、LLM 调用、
事件发射的实现细节——全部委托给注入的协作者。
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.config import get_config
from wing.hook_registry import hooks
from wing.request_context import reset_request_context, set_request_context
from wing.schema import ContentBlock, LLMUsage, Message

from .event_sink import AgentEventSink
from .inbox import Inbox
from .tool_executor import InterruptedToolResults, ToolExecutor

if TYPE_CHECKING:
    from wing.context_manager import ContextManager
    from wing.provider.base import ModelProvider, StreamAccumulator
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
    """ReAct 主循环编排器。

    职责包括 LLM chunk 流消费与 assistant Message 组装（`_call_llm`）——
    注意边界：实际模型调用在 `ModelProvider.generate()`，loop 侧只消费流、
    经 sink 发射流式事件、以 provider 产出的权威块数组组装 Message。
    """

    def __init__(
        self,
        tool_executor: ToolExecutor,
        sink: AgentEventSink,
        context_manager: ContextManager,
        inbox: Inbox,
        current_model: Callable[[], str],
        current_provider: Callable[[], "ModelProvider"],
        current_tools: Callable[[], list[Tool]],
        stream: bool = True,
        set_working: Callable[[bool], None] | None = None,
    ) -> None:
        self._executor = tool_executor
        self._sink = sink
        self._cm = context_manager
        self._inbox = inbox
        self._current_model = current_model
        self._current_provider = current_provider
        self._current_tools = current_tools
        self._stream = stream
        self._set_working = set_working
        # 运行时可变配置（由 WingAgent 设置）
        self.max_turns: int | None = None
        self.steer: bool = get_config().steer
        # 当前一轮 LLM 调用的流累积状态（未提交内容的唯一权威）。
        # _call_llm 入口新建并登记，轮提交/中断补提交/turn 收口后置空。
        # 未提交投影（uncommitted_message / uncommitted_tools）按需快照它，
        # 不缓存副本。
        self._current_acc: "StreamAccumulator | None" = None

    @property
    def current_acc(self) -> "StreamAccumulator | None":
        """当前一轮 LLM 调用的流累积状态（None 表示无进行中轮次）。"""
        return self._current_acc

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
            # 消费确认：被合并进本轮输入的用户消息在此正式"被模型接受"。
            # 前端据此把排队中的消息上移进聊天历史。只对外部投递发射
            # （request_id 存在）；内部投递（如 Explorer 回传）不发射。
            # 先于 turn_started——事件流顺序即渲染顺序。
            for b in batch:
                if b.request_id and b.message.role == "user" and b.message.content:
                    self._sink.user_message_accepted(b.message.content, b.request_id)
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
            # 兜底置空当前 accumulator（错误/取消路径轮边界未触达时；
            # 正常路径轮边界已置空，此处幂等）。未提交投影随之失效——
            # turn 收口后不再有"进行中内容"。
            self._current_acc = None
            if self._set_working:
                self._set_working(False)
            reset_request_context(token)

    async def _llm_turn(self, ctx: _TurnAccumulator) -> bool:
        """执行一轮 LLM 生成 + 工具执行。

        Returns: True 需要继续下一轮，False 对话结束。
        """
        # model / provider 从唯一存储（WingAgent）调用点取值
        model = self._current_model()
        provider = self._current_provider()

        # LLM 调用
        llm_result = await self._cm.get_messages_for_llm(
            model=model,
            model_provider=provider,
            current_tools=self._current_tools,
        )
        assistant_msg = await self._call_llm(
            provider=provider,
            messages=llm_result.messages,
            model=model,
            tools=llm_result.tools,
        )

        # Emit turn-level AssistantTurnEvent
        self._sink.assistant_turn(assistant_msg, model)

        # Update accumulator
        ctx.record_usage(assistant_msg.usage)
        ctx.record_text(assistant_msg.content)

        # 工具执行
        pending_tool_calls = assistant_msg.tool_calls or []
        try:
            tc_results = await self._executor.execute(pending_tool_calls, model)
            interrupted_exc: InterruptedToolResults | None = None
        except InterruptedToolResults as e:
            tc_results = e.results
            interrupted_exc = e

        # Steer: drain inbox 中积攒的用户消息。每条被消费的消息都是在此刻
        # 真正"被模型接受"——逐条发射 UserMessageAcceptedEvent，前端据此
        # 把排队消息上移（落在工具结果之后、下一轮输出之前）。
        if self.steer and tc_results:
            steer_items = self._inbox.drain_for_steer()
            if steer_items:
                for b in steer_items:
                    if b.request_id and b.message.content:
                        self._sink.user_message_accepted(
                            b.message.content, b.request_id
                        )
                steer_notes = self._inbox.format_steer_note(steer_items)
                tc_results[-1].content = steer_notes + (tc_results[-1].content or "")

        # 提交本轮消息
        turn_messages = [assistant_msg] + tc_results
        self._cm.add_messages(turn_messages)

        # 轮边界：本轮内容已提交进链（assistant Message + tool 结果），
        # 当前 accumulator 置空——未提交投影失效，避免中途订阅者把已提交
        # 内容经投影再渲染一次（多轮 turn 的双重渲染）。
        self._current_acc = None

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
            return False
        return True

    async def _call_llm(
        self,
        provider: "ModelProvider",
        messages: list[Message],
        model: str,
        tools: list[Tool] | None,
    ) -> Message:
        """消费 provider 的 chunk 流：发射流式事件，组装 assistant Message。

        实际模型调用在 provider.generate()；本方法只消费流 + 发射事件 +
        组装 Message。两个 provider 统一在最终 chunk 产出权威 content_blocks，
        是 Message 组装的唯一依据；provider 未产出块数组（流未正常结束）
        视为契约违反并报错——截断轮次 MUST NOT 作为成功 turn 提交，无扁平兜底。

        中断补提交：流式生成期间被取消（CancelledError 直通——with_retry
        只捕 Exception）时，从 caller 持有的 accumulator 快照已累积的部分块
        （text/thinking 任意长度保留，未终结 tool 块由 provider 剔除），
        有内容则组装 partial assistant Message 提交进上下文，然后 re-raise
        让 worker 终止。用户可放心打断长思考——已花费 tokens 的内容不丢。
        """
        content_blocks: list[ContentBlock] | None = None
        last_usage: LLMUsage | None = None
        # accumulator 上提为 turn 级持有（self._current_acc）——未提交投影
        # （resume）与中断补提交按需快照同一个对象，不再是 _call_llm 局部变量。
        accumulator = provider.create_accumulator()
        self._current_acc = accumulator

        try:
            async for chunk in provider.generate(
                messages=messages,
                model=model,
                tools=tools,
                stream=self._stream,
                accumulator=accumulator,
            ):
                if chunk.reasoning_content:
                    self._sink.llm_reasoning(chunk.reasoning_content)

                if chunk.content:
                    self._sink.llm_text(chunk.content)

                if chunk.content_blocks is not None:
                    # provider 产出的权威块数组（最终 chunk 携带）
                    content_blocks = chunk.content_blocks

                if chunk.tool_call_deltas:
                    for delta in chunk.tool_call_deltas:
                        self._sink.llm_tool_call_delta(
                            tool_call_id=delta.id,
                            tool_name=delta.name,
                            args_fragment=delta.args_fragment,
                            is_final=delta.is_final,
                        )

                if chunk.usage.completion_tokens or chunk.usage.prompt_tokens:
                    last_usage = chunk.usage
                    self._sink.llm_metrics(chunk.usage)
        except asyncio.CancelledError:
            # 中断补提交：快照当前 accumulator（与 resume 未提交投影同源）。
            partial_blocks = provider.snapshot_blocks(self._current_acc)
            if partial_blocks:
                partial_msg = Message(
                    role="assistant",
                    content_blocks=partial_blocks,
                    usage=last_usage,
                    stop_reason="interrupted",
                )
                self._cm.add_message(partial_msg)
                # 补提交完成：内容已进链，未提交投影失效（不再双重渲染）。
                self._current_acc = None
            raise

        if content_blocks is None:
            raise RuntimeError(
                "provider did not emit authoritative content_blocks "
                "(stream ended without a complete block array)"
            )
        return Message(
            role="assistant",
            content_blocks=content_blocks,
            usage=last_usage,
            stop_reason=last_usage.stop_reason if last_usage else None,
        )
