# wing/agent/llm_caller.py
"""LLMCaller — LLM 流式调用 + chunk 消费。

职责单一：调用 provider.generate()，消费 chunk 流，通过 AgentEventSink
发射流式事件，返回完整的 assistant Message。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.schema import ContentBlock, LLMUsage, Message, ToolCall

from .event_sink import AgentEventSink

if TYPE_CHECKING:
    from wing.provider.base import ModelProvider
    from wing.schema import Tool


class LLMCaller:
    """LLM 流式调用器——消费 chunk 流，产出完整 assistant Message。"""

    def __init__(self, model_provider: ModelProvider, sink: AgentEventSink) -> None:
        self._provider = model_provider
        self._sink = sink

    def set_provider(self, provider: ModelProvider) -> None:
        """切换底层 provider（运行时切模型时由 WingAgent 委托）。"""
        self._provider = provider

    async def call(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None,
        stream: bool,
    ) -> Message:
        """执行一次 LLM 调用，返回完整 assistant Message。

        流式事件（text、reasoning、tool_call_stream、metrics）通过 sink 发射。
        """
        content_chunks: list[str] = []
        reasoning_chunks: list[str] = []
        pending_tool_calls: list[ToolCall] = []
        content_blocks: list[ContentBlock] | None = None
        last_usage: LLMUsage | None = None

        async for chunk in self._provider.generate(
            messages=messages,
            model=model,
            tools=tools,
            stream=stream,
        ):
            if chunk.reasoning_content:
                reasoning_chunks.append(chunk.reasoning_content)
                self._sink.llm_reasoning(chunk.reasoning_content)

            if chunk.content:
                content_chunks.append(chunk.content)
                self._sink.llm_text(chunk.content)

            if chunk.tool_calls:
                pending_tool_calls.extend(chunk.tool_calls)

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

        log.info(
            f"LLM call complete: {len(content_chunks)} text chunks, "
            f"{len(pending_tool_calls)} tool calls"
        )

        if content_blocks is not None:
            # provider 产出权威块数组：作为真相源，扁平字段由其填充
            msg = Message(
                role="assistant", content_blocks=content_blocks, usage=last_usage
            )
            msg.sync_flat_from_blocks()
            return msg

        # 兜底：provider 未给块数组，从累积的扁平 chunk 构建
        return Message(
            role="assistant",
            content="".join(content_chunks),
            reasoning_content="".join(reasoning_chunks),
            tool_calls=pending_tool_calls,
            usage=last_usage,
        )
