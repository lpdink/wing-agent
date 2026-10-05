# wing/provider/anthropic/stream.py
"""Anthropic 流状态机与事件处理器（content_block_* / message_*）。

``_StreamState`` 与其 handler 同住本文件——状态按事件 index 结构化累积，
handler 读写同一状态；provider 侧经 ``_StreamMixin`` 合体使用。
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field

from wing.common.logger import log
from wing.provider.base import parse_tool_args
from wing.schema import (
    LLMResponse,
    LLMUsage,
    PendingCall,
    TextBlock,
    ThinkingBlock,
    ToolCall,
    ToolCallDelta,
    ToolUseBlock,
)


@dataclass
class _StreamState:
    """Anthropic 流式解析状态——per-index 结构化。

    块与 tool 参数均按事件 index 独立累积，不依赖"块事件严格顺序"的隐式
    假设：交错到达的 delta 各归其 index，content_block_stop 按其 index
    终结对应块。
    """

    blocks_by_index: dict[int, TextBlock | ThinkingBlock | ToolUseBlock] = field(
        default_factory=dict
    )
    pending_tools: dict[int, PendingCall] = field(default_factory=dict)
    # usage 累计
    prompt_tokens: int = 0
    completion_tokens: int = 0
    cached_tokens: int = 0
    cache_creation: int = 0
    input_source: str = "start"  # input_tokens 来源：delta（权威）或 start（兜底）
    first_token_ts: float | None = None
    # 终止原因（message_delta.delta.stop_reason：end_turn/max_tokens/tool_use/…）
    stop_reason: str | None = None


def _is_zero_info_block(block: object) -> bool:
    """零信息块：取消恰逢 content_block_start 与首个 delta 之间的产物。

    空 text 块与「无文本、无签名、非 redacted」的 thinking 块携带零信息，
    落链/进权威块数组会产出空 assistant 消息。带 signature / redacted 的
    空文本 thinking 携带不可重建信息，必须保留。
    """
    if isinstance(block, TextBlock):
        return not block.text
    if isinstance(block, ThinkingBlock):
        return not block.redacted and not block.thinking.strip() and not block.signature
    return False


class _StreamMixin:
    """``AnthropicProvider`` 的流状态机与事件处理方法组。"""

    @staticmethod
    def _ordered_finalized_blocks(state: _StreamState) -> list:
        """按 index 排序产出已终结的块（跳过仍在 pending 的 tool 块）。

        同时服务于 message_stop 的权威块数组产出（max_tokens 截断在
        tool args 中间时，半截 tool_use 不得进入）与中断快照。零信息块
        （见 _is_zero_info_block）一并剔除。
        """
        blocks = []
        for idx in sorted(state.blocks_by_index):
            if idx in state.pending_tools:
                continue
            block = state.blocks_by_index[idx]
            if _is_zero_info_block(block):
                continue
            blocks.append(block)
        return blocks

    # ─── Stream Event Handlers ────────────────────────────────────

    @staticmethod
    def _on_block_start(state: _StreamState, data: dict) -> None:
        idx = data.get("index", 0)
        block = data.get("content_block", {})
        btype = block.get("type", "")
        if btype == "thinking":
            state.blocks_by_index[idx] = ThinkingBlock(thinking="", signature="")
        elif btype == "redacted_thinking":
            # 加密 payload 当不透明黑盒，存进 signature，redacted=True
            state.blocks_by_index[idx] = ThinkingBlock(
                thinking="", signature=block.get("data", ""), redacted=True
            )
        elif btype == "text":
            state.blocks_by_index[idx] = TextBlock(text="")
        elif btype == "tool_use":
            tool_id = block.get("id", "")
            tool_name = block.get("name", "")
            state.blocks_by_index[idx] = ToolUseBlock(
                id=tool_id, name=tool_name, input={}
            )
            state.pending_tools[idx] = PendingCall(id=tool_id, name=tool_name)
            # 工具名同样是解码产出，且无参工具的部分服务端不下发
            # input_json_delta（见 parse_tool_args 契约）：块起点打点兜底，
            # 与 OpenAI 首片 id / name 即打点的口径对齐。
            if state.first_token_ts is None:
                state.first_token_ts = time.monotonic()

    @staticmethod
    def _on_block_delta(
        state: _StreamState,
        data: dict,
        model: str,
        request_id: str,
        first_chunk_rt_ms: float,
    ) -> LLMResponse | None:
        """处理 content_block_delta：累积进对应 index 的块，发射增量事件。"""
        idx = data.get("index", 0)
        delta = data.get("delta", {})
        delta_type = delta.get("type", "")
        meta = LLMUsage(
            first_chunk_rt_ms=first_chunk_rt_ms, model=model, request_id=request_id
        )

        if delta_type == "text_delta":
            text = delta.get("text", "")
            if state.first_token_ts is None and text:
                state.first_token_ts = time.monotonic()
            blk = state.blocks_by_index.get(idx)
            if isinstance(blk, TextBlock):
                blk.text += text
            return LLMResponse(content=text, usage=meta)

        if delta_type == "thinking_delta":
            thinking = delta.get("thinking", "")
            if state.first_token_ts is None and thinking:
                state.first_token_ts = time.monotonic()
            blk = state.blocks_by_index.get(idx)
            if isinstance(blk, ThinkingBlock):
                blk.thinking += thinking
            return LLMResponse(reasoning_content=thinking, usage=meta)

        if delta_type == "signature_delta":
            # per-block signature：累加进对应块（兼容单片/多片）
            sig = delta.get("signature", "")
            blk = state.blocks_by_index.get(idx)
            if isinstance(blk, ThinkingBlock) and sig:
                blk.signature = (blk.signature or "") + sig
            return None

        if delta_type == "input_json_delta":
            call = state.pending_tools.get(idx)
            if call is None:
                return None
            call.args_buffer += delta.get("partial_json", "")
            fragment = call.args_buffer[call.emitted_len :]
            if not fragment or not call.id:
                return None
            call.emitted_len = len(call.args_buffer)
            return LLMResponse(
                tool_call_deltas=[
                    ToolCallDelta(
                        id=call.id,
                        name=call.name,
                        args_fragment=fragment,
                        is_final=False,
                    )
                ],
                usage=meta,
            )

        return None

    @staticmethod
    def _on_block_stop(
        state: _StreamState,
        data: dict,
        model: str,
        request_id: str,
        first_chunk_rt_ms: float,
    ) -> LLMResponse | None:
        """content_block_stop：按其 index 终结 tool 块（权威 JSON 解析）。"""
        idx = data.get("index", 0)
        call = state.pending_tools.pop(idx, None)
        if call is None or not call.id:
            return None
        # 容错解析与 OpenAI 路径同构：非法 JSON 不抛异常、也不静默吞掉，
        # 置 arguments_error 由执行器短路回灌给模型自纠。
        args, args_error = parse_tool_args(call.args_buffer)
        blk = state.blocks_by_index.get(idx)
        if isinstance(blk, ToolUseBlock):
            blk.input = args
            blk.input_error = args_error
        return LLMResponse(
            tool_calls=[
                ToolCall(
                    id=call.id,
                    name=call.name,
                    arguments=args,
                    arguments_error=args_error,
                )
            ],
            tool_call_deltas=[
                ToolCallDelta(
                    id=call.id,
                    name=call.name,
                    args_fragment=call.args_buffer[call.emitted_len :],
                    is_final=True,
                )
            ],
            usage=LLMUsage(
                first_chunk_rt_ms=first_chunk_rt_ms, model=model, request_id=request_id
            ),
        )

    @staticmethod
    def _on_message_delta(state: _StreamState, data: dict) -> None:
        # 权威累计 usage（官方文档：message_delta 的 usage 为 cumulative，
        # 且当前版本 API 含 input_tokens；message_start 仅作兜底）。
        usage_delta = data.get("usage", {})
        log.debug(f"[anthropic usage] message_delta raw: {usage_delta}")
        if "input_tokens" in usage_delta:
            state.prompt_tokens = usage_delta["input_tokens"]
            state.input_source = "delta"
        if "output_tokens" in usage_delta:
            state.completion_tokens = usage_delta["output_tokens"]
        if "cache_read_input_tokens" in usage_delta:
            state.cached_tokens = usage_delta["cache_read_input_tokens"]
        if "cache_creation_input_tokens" in usage_delta:
            state.cache_creation = usage_delta["cache_creation_input_tokens"]
        # 终止原因（end_turn / max_tokens / tool_use / stop_sequence）
        stop_reason = data.get("delta", {}).get("stop_reason")
        if stop_reason:
            state.stop_reason = stop_reason

    @staticmethod
    def _on_message_start(state: _StreamState, data: dict) -> None:
        # 兜底：标准 Anthropic 把完整 usage（含 cache_creation/cache_read）
        # 放在 message_start，message_delta 仅含 output_tokens；部分代理则
        # 相反（见 message_delta 分支）。两处都读，谁带就用谁，delta 后到覆盖。
        usage_start = data.get("message", {}).get("usage", {})
        log.debug(f"[anthropic usage] message_start raw: {usage_start}")
        if "input_tokens" in usage_start:
            state.prompt_tokens = usage_start["input_tokens"]
            state.input_source = "start"
        if "cache_read_input_tokens" in usage_start:
            state.cached_tokens = usage_start["cache_read_input_tokens"]
        if "cache_creation_input_tokens" in usage_start:
            state.cache_creation = usage_start["cache_creation_input_tokens"]

    @staticmethod
    def _build_final_response(
        state: _StreamState, model: str, request_id: str, first_chunk_rt_ms: float
    ) -> LLMResponse:
        """message_stop：产出有序 content_blocks（权威块数组）+ 最终 usage。

        未被 content_block_stop 终结的 tool 块（max_tokens 砍在参数中间）
        从块数组中剔除——半截 tool_use 不产生工具调用。零信息块过滤后为
        空数组时原样输出 []（合法协议输出，与 OpenAI 路径统一；轮有效性由
        ReActLoop 判定——空数组会被判无效并重试）——None 保留
        给「流未正常结束」的契约违反信号。
        """
        # Anthropic 的 input_tokens 仅为非缓存部分（含兜底/增量源）；
        # 对齐 OpenAI 语义：prompt_tokens = 总输入（含缓存）
        total_prompt = state.prompt_tokens + state.cached_tokens + state.cache_creation
        hit_rate = state.cached_tokens / total_prompt * 100 if total_prompt else 0.0
        log.debug(
            f"[anthropic usage] final: "
            f"input(non-cached)={state.prompt_tokens}({state.input_source}) "
            f"cache_read={state.cached_tokens} "
            f"cache_creation={state.cache_creation} "
            f"output={state.completion_tokens} => "
            f"prompt_tokens(total)={total_prompt} "
            f"hit_rate={hit_rate:.1f}%"
        )
        decode_tps = 0.0
        if state.completion_tokens > 0 and state.first_token_ts is not None:
            decode_elapsed = time.monotonic() - state.first_token_ts
            if decode_elapsed > 0:
                decode_tps = state.completion_tokens / decode_elapsed
        ordered_blocks = _StreamMixin._ordered_finalized_blocks(state)
        return LLMResponse(
            content_blocks=ordered_blocks,
            usage=LLMUsage(
                prompt_tokens=total_prompt,
                completion_tokens=state.completion_tokens,
                cached_tokens=state.cached_tokens,
                first_chunk_rt_ms=first_chunk_rt_ms,
                tokens_per_sec=decode_tps,
                model=model,
                request_id=request_id,
                stop_reason=state.stop_reason,
            ),
        )
