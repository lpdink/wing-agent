# wing/provider/openai/stream.py
"""OpenAI 兼容协议的流状态与处理器（tool delta 累积 / 权威块数组组装）。

``_OAIStreamState`` 与 ``_process_tool_deltas`` 同住本文件；逐 chunk 的累积在
``provider`` 的流式泵里内联完成——本步骤不重排该结构。
"""

from __future__ import annotations

from dataclasses import dataclass, field

from wing.provider.base import parse_tool_args
from wing.schema import (
    ContentBlock,
    PendingCall,
    TextBlock,
    ThinkingBlock,
    ToolCall,
    ToolCallDelta,
    ToolUseBlock,
)


@dataclass
class _OAIStreamState:
    """OpenAI-compat 流式累积状态（中断补提交的快照源）。

    final_tool_calls 只含已终结的调用（finish_reason=tool_calls 到达时
    解析入列）；pending 中未终结的调用在 snapshot 时丢弃——半截参数
    不可解析且无配对结果。
    """

    reasoning_chunks: list[str] = field(default_factory=list)
    content_chunks: list[str] = field(default_factory=list)
    final_tool_calls: list[ToolCall] = field(default_factory=list)
    pending: dict[int, PendingCall] = field(default_factory=dict)
    first_token_ts: float | None = None
    stop_reason: str | None = None


class _StreamMixin:
    """``OpenAICompatProvider`` 的流状态处理器方法组（与 provider 类合体后生效）。"""

    @staticmethod
    def _build_content_blocks(
        reasoning: str | None,
        content: str | None,
        tool_calls: list[ToolCall],
    ) -> list[ContentBlock]:
        """从 OpenAI 扁平响应构建权威块数组（与 Anthropic 路径输出契约统一）。

        OpenAI 扁平协议无块序信息，约定序为 thinking → text → tool_use；
        reasoning 映射为无签名 ThinkingBlock（OpenAI 协议无签名概念）。
        """
        blocks: list[ContentBlock] = []
        if reasoning:
            blocks.append(ThinkingBlock(thinking=reasoning))
        if content:
            blocks.append(TextBlock(text=content))
        for tc in tool_calls:
            blocks.append(
                ToolUseBlock(
                    id=tc.id,
                    name=tc.name,
                    input=tc.arguments,
                    input_error=tc.arguments_error,
                )
            )
        return blocks

    # ─── Tool Delta Processing ────────────────────────────────────

    @staticmethod
    def _process_tool_deltas(
        choice: dict,
        pending: dict[int, PendingCall],
    ) -> tuple[list[ToolCall] | None, list[ToolCallDelta] | None]:
        """Process tool call deltas from a streaming chunk dict.

        Returns:
            (final_tool_calls, streaming_deltas)
        """
        delta = choice.get("delta", {})
        tc_deltas = delta.get("tool_calls")
        has_tool_delta = bool(tc_deltas)

        for tc in tc_deltas or []:
            idx = tc.get("index", 0)
            call = pending.setdefault(idx, PendingCall())
            if tc.get("id"):
                call.id = tc["id"]
            func = tc.get("function") or {}
            if func.get("name"):
                call.name = func["name"]
            if func.get("arguments"):
                call.args_buffer += func["arguments"]

        deltas: list[ToolCallDelta] | None = None
        if has_tool_delta and pending:
            is_final = choice.get("finish_reason") == "tool_calls"
            deltas = []
            for call in pending.values():
                fragment = call.args_buffer[call.emitted_len :]
                if not call.id or not fragment:
                    continue
                call.emitted_len = len(call.args_buffer)
                deltas.append(
                    ToolCallDelta(
                        id=call.id,
                        name=call.name,
                        args_fragment=fragment,
                        is_final=is_final,
                    )
                )
            if not deltas:
                deltas = None

        finals: list[ToolCall] | None = None
        if choice.get("finish_reason") == "tool_calls":
            # 容错解析：笨模型的 args JSON 可能非法——绝不抛异常（会触发
            # 整轮重试并丢弃已生成内容），置 arguments_error 由执行器短路
            # 回灌给模型自纠。
            finals = [
                ToolCall(
                    id=call.id,
                    name=call.name,
                    arguments=args,
                    arguments_error=args_error,
                )
                for call in pending.values()
                for args, args_error in [parse_tool_args(call.args_buffer)]
            ]
            pending.clear()

        return finals, deltas
