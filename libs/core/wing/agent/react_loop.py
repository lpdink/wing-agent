# wing/agent/react_loop.py
"""ReActLoop — 主循环编排。

职责单一：drain inbox → hook → add message → while loop（LLM → tools →
steer → commit）→ emit TurnResult/Done。不持有工具执行、LLM 调用、
事件发射的实现细节——全部委托给注入的协作者。

轮有效性：单轮 LLM 生成若“不进入下一次 ReAct 且不合法”（无 content 且无
收敛的 tool call，或 content 在但 tool call 起了头全未收敛），视为无效轮次
——交给 `with_retry` 有界重试，绝不作为成功 turn 提交。判据与提交口径见
`_call_llm_validated`。
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.common.with_retry import with_retry
from wing.config import get_config
from wing.hook_registry import hooks
from wing.request_context import reset_request_context, set_request_context
from wing.schema import ContentBlock, LLMUsage, Message

from .event_sink import AgentEventSink
from .inbox import Inbox
from .tool_executor import (
    INTERRUPTED_RESULT,
    InterruptedToolResults,
    ToolExecutor,
)

if TYPE_CHECKING:
    from wing.context_manager import ContextManager
    from wing.provider.base import ModelProvider, StreamAccumulator
    from wing.schema import Tool


class InvalidGenerationError(RuntimeError):
    """本轮生成无效——不进入下一次 ReAct 且不合法。

    判据（见 `ReActLoop._call_llm_validated`）：
    - 无 content（reasoning 不算）且无收敛的 tool call（全空 / 只有 reasoning）；
    - 有 content，但 tool call 起了头、一个都没收敛（流被上游截断）。

    抛出后由 `with_retry(retry_on=(InvalidGenerationError,))` 有界重试；
    重试耗尽沿 run_turn 错误路径上报——无效轮次绝不作为成功 turn 提交。
    """


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
        # 运行时可变配置（由 WingAgent 在构造与 provider 切换时同步）
        self.max_turns: int | None = None
        self.steer: bool = get_config().steer
        # `with_retry` 参数解析源（装饰器经实例的 `_config` 读取
        # max_retries / max_retry_delay）：WingAgent 同步当前 provider 的
        # `provider.config`；None → 默认口径（10 次重试、3s 起步指数退避、
        # 封顶 180s）。测试可注入小值以避免真实等待。
        self._config: object | None = None
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

        # LLM 调用（含轮有效性校验与无效重试，见 _call_llm_validated）
        assistant_msg = await self._call_llm_validated(
            ctx, provider=provider, model=model
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

    # ── 生成有效性（无效轮次重试）─────────────────

    @with_retry(label="模型生成", retry_on=(InvalidGenerationError,))
    async def _call_llm_validated(
        self, ctx: "_TurnAccumulator", provider: "ModelProvider", model: str
    ) -> Message:
        """一次 LLM 生成 + 轮有效性校验；无效则抛错交给装饰器有界重试。

        有效性规则（上游「空响应 / 截断」的容错——判定放 loop 层而非
        provider：无效重试需要在重试前把已生成的 content 提交进链，provider
        层的整轮重发做不到）：

        1. 有任一已收敛（provider 已终结）的 tool call → 有效，绝不重试；
           其余未收敛调用随现状剔除，不阻塞自然进入下一轮。
        2. 无收敛 tool call 且无 content → 无效（含全空与只有 reasoning 的
           情况——reasoning 不作为收尾依据），不提交任何内容，重试。
        3. 有 content 但存在未收敛的 tool call（流被截断）→ content 提交、
           tool call 不提交（与 provider 剔除口径一致），然后重试。
        4. 有 content 且无 tool call 尝试 → 正常收尾，不重试。
        另：流未正常结束（无权威块数组）时 `_call_llm` 抛同型异常，一并
        落入重试（如 Anthropic 在 message_stop 前被切断）。

        每次尝试重新取 `get_messages_for_llm`——规则 3 的重试请求必须带上
        刚提交的 content。无效尝试置空 `_current_acc`：被丢弃的内容不进
        未提交投影（中途订阅者不会看到将被重试覆盖的内容），其 usage 照常
        计入 turn 账（token 真花掉了；provider 未产出块数组的尝试除外——
        该路径无 usage 可读）。

        已知边界（待真实 Anthropic 环境验证后决策）：规则 3 的重试请求以
        已提交的 assistant 消息结尾（续跑语义）——OpenAI 兼容协议即
        continuation；Anthropic 开 thinking 时 prefill 可能被拒（则该轮
        重试失败、走错误路径，内容不丢）。
        """
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

        if assistant_msg.tool_calls:
            return assistant_msg

        ctx.record_usage(assistant_msg.usage)

        if not (assistant_msg.content or "").strip():
            self._current_acc = None
            raise InvalidGenerationError(
                "模型本轮未产出 content，也没有收敛的 tool call"
                "（空响应 / 流被截断；reasoning 不作为收尾依据）"
            )

        if self._has_unfinished_tool_calls(provider):
            self._cm.add_messages([assistant_msg])
            self._current_acc = None
            raise InvalidGenerationError(
                "模型本轮有 content 但 tool call 未收敛（流被截断）："
                "content 已提交，未收敛的 tool call 未提交"
            )

        return assistant_msg

    def _has_unfinished_tool_calls(self, provider: "ModelProvider") -> bool:
        """本轮流结束时是否残留未终结的 tool call（截断检测）。

        计数口径（`unfinished_tool_calls`）含无 id 的半截调用——不依赖
        投影 API 的 id 过滤。sync 路径无累积状态，恒为 0。
        """
        return provider.unfinished_tool_calls(self._current_acc) > 0

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

        已终结但未执行的 tool_use 不能悬空落链：Anthropic 要求 tool_use 后
        紧跟 tool_result、OpenAI 要求 tool_calls 后有 tool 消息，否则下一轮
        请求 400。为每个已终结调用合成打断结果（与工具执行期打断的合成
        语义一致，卡片保留、配对合法）。
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
                # 已终结未执行的 tool_use → 合成打断结果补配对（先落链，
                # 再广播关卡片事件——日志是事实源，广播是投影）。
                interrupted_calls = partial_msg.tool_calls or []
                tool_results = [
                    Message(
                        role="tool",
                        tool_call_id=tc.id,
                        content=INTERRUPTED_RESULT,
                    )
                    for tc in interrupted_calls
                ]
                self._cm.add_messages([partial_msg] + tool_results)
                for tc in interrupted_calls:
                    self._sink.tool_finished(
                        tc, INTERRUPTED_RESULT, success=False, model=model
                    )
                # 补提交完成：内容已进链，未提交投影失效（不再双重渲染）。
                self._current_acc = None
            raise

        if content_blocks is None:
            # 流未正常结束（无权威块数组）：无效轮次——由 _call_llm_validated
            # 的有界重试收口（Anthropic 在 message_stop 前被切断即此形态）。
            self._current_acc = None
            raise InvalidGenerationError(
                "provider did not emit authoritative content_blocks "
                "(stream ended without a complete block array)"
            )
        return Message(
            role="assistant",
            content_blocks=content_blocks,
            usage=last_usage,
            stop_reason=last_usage.stop_reason if last_usage else None,
        )
