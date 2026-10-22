# wing/schema/llm.py
from __future__ import annotations

import json
from typing import overload

from pydantic import BaseModel


class ToolCall(BaseModel):
    id: str
    name: str
    arguments: dict
    arguments_error: str | None = None
    """工具参数解析失败的错误描述（含原始参数文本）。

    模型吐出的 args JSON 非法（如尾逗号）时，provider 不抛异常，而是
    置 arguments={} 并在此记录现场。ToolExecutor 见它短路执行，把错误
    作为工具结果回灌给模型自纠，而非整轮重试丢弃。
    """

    def to_openai(self) -> dict:
        """Convert to OpenAI tool_calls format."""
        return {
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": json.dumps(self.arguments, ensure_ascii=False),
            },
        }

    def __repr__(self) -> str:
        args = json.dumps(self.arguments, ensure_ascii=False)
        return f"ToolCall({self.name}({args}))"


class LLMUsage(BaseModel):
    prompt_tokens: int = 0
    completion_tokens: int = 0
    cached_tokens: int = 0
    """prompt 中被缓存命中的 token 数"""
    first_chunk_rt_ms: float = 0.0
    """首 token RT（毫秒）：请求发出 → 首个正文/思考/tool 增量到达。

    流式路径的耗时口径（前端 ttft / 审计的 first_chunk_rt_ms_*）。流内尚无
    生成（空响应 / 立即失败）时退回首包（响应头）RT；非流式没有逐 token 时序，
    退化为整个请求耗时。"""
    tokens_per_sec: float = 0.0
    """流式输出 tokens/s（基于服务端返回的 completion_tokens 精准计算）"""
    model: str = ""
    """模型名称，由 provider 在构建 usage 时注入"""
    request_id: str = ""
    """LLM API 响应的 x-request-id，用于排查问题"""
    stop_reason: str | None = None
    """终止原因（协议原值：end_turn / max_tokens / tool_use / stop / length…）。

    provider 在响应帧的 usage 上设置（OpenAI 兼容：带内 usage 帧带"此刻
    已知值"、零 token 的流尾终帧带权威值）；react_loop **按帧全量取值**
    ——不随 metrics 的 token 过滤丢帧——传导进 Message.stop_reason（唯一
    落盘审计位置）与 LLMCallMetricsEvent。本字段是随行快照：OpenAI 带内帧
    可能缺（此时只有 Message 顶层字段有值）。中断路径由 runtime 合成
    "interrupted"。"""

    def __repr__(self) -> str:
        parts = [f"in:{self.prompt_tokens} out:{self.completion_tokens}"]
        if self.model:
            parts.insert(0, f"model:{self.model}")
        if self.cached_tokens:
            parts.append(f"cached:{self.cached_tokens}")
        if self.first_chunk_rt_ms:
            parts.append(f"rt:{self.first_chunk_rt_ms:.0f}ms")
        if self.tokens_per_sec:
            parts.append(f"speed:{self.tokens_per_sec:.1f}t/s")
        return " ".join(parts)


class ToolCallDelta(BaseModel):
    """Incremental raw args fragment emitted during streaming.

    Carries only the args text accumulated since the last delta for this
    call (the first delta carries the full prefix). Clients accumulate
    fragments and partial-parse locally — the runtime never parses
    partial args.
    """

    id: str
    name: str
    args_fragment: str = ""
    is_final: bool = False


class LLMResponse(BaseModel):
    content: str | None = None
    reasoning_content: str | None = None
    content_blocks: list[ContentBlock] | None = None
    """provider 产出的结构化块数组（权威）。流式结束时由 provider 在最终
    chunk 给出，ReActLoop 组装进 Message。"""
    tool_calls: list[ToolCall] | None = None
    tool_call_deltas: list[ToolCallDelta] | None = None
    usage: LLMUsage = LLMUsage()


@overload
def usage_wire(usage: LLMUsage) -> dict[str, int]: ...


@overload
def usage_wire(usage: None) -> None: ...


def usage_wire(usage: LLMUsage | None) -> dict[str, int] | None:
    """``LLMUsage`` → 协议 wire dict（``TurnResultEvent.usage`` / ``AssistantTurnEvent.usage``）。

    这三个键是**对外字段名**（stdio / SDK 消费），因此形状只在这里定义：加字段时
    两个事件的来源不会各改一半。
    """
    if usage is None:
        return None
    return {
        "input_tokens": usage.prompt_tokens,
        "output_tokens": usage.completion_tokens,
        "cached_tokens": usage.cached_tokens,
    }


class PendingCall(BaseModel):
    id: str = ""
    name: str = ""
    args_buffer: str = ""
    emitted_len: int = 0
    """args_buffer 中已作为流式碎片 emit 的长度（增量游标）。"""


# LLMResponse.content_blocks 引用 message.ContentBlock；延迟导入 + rebuild，
# 与 message.py 底部的延迟导入对称（包内导入顺序不敏感）。
from .message import ContentBlock  # noqa: E402

LLMResponse.model_rebuild()
