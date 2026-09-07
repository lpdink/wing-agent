# wing/provider/base.py
"""ModelProvider ABC — 模型调用层的统一接口。

所有协议实现（OpenAI 兼容、Anthropic）继承此基类，
输出统一的 LLMResponse 流，上层 ReActLoop 对协议无感知。
"""

from __future__ import annotations

import json
from abc import ABC, abstractmethod
from typing import TYPE_CHECKING, AsyncIterator

from wing.schema import LLMResponse, Message, Tool

if TYPE_CHECKING:
    from wing.config import ProviderConfig


def parse_tool_args(raw: str) -> tuple[dict, str | None]:
    """解析工具调用原始 args JSON——永不抛异常。

    笨模型的 args JSON 可能非法（尾逗号、未转义引号等）。解析失败时
    MUST NOT 让异常逃逸出流——那会触发整轮重试、丢弃已生成的
    thinking/content/tool call。正确路径：返回 ({}, 错误现场)，由
    ToolExecutor 短路执行并把错误作为工具结果回灌给模型自纠。

    Returns:
        (arguments, arguments_error)：
        - 成功 → (解析出的 dict, None)
        - 空/空白串 → ({}, None)——无参工具的部分服务端不下发 "{}"
        - 非法 JSON 或非 object → ({}, 错误描述 + 完整原始文本)
    """
    if not raw or not raw.strip():
        return {}, None
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as e:
        return {}, (
            f"Invalid JSON in tool call arguments: {e}. Arguments received:\n{raw}"
        )
    if not isinstance(parsed, dict):
        return {}, (
            f"Tool call arguments must be a JSON object, "
            f"got {type(parsed).__name__}. Arguments received:\n{raw}"
        )
    return parsed, None


class StreamAccumulator:
    """caller 持有的流累积状态容器（provider 协议中立的不透明壳）。

    provider.generate(..., accumulator=...) 在每次尝试开始时把内部流
    状态装入 holder.state（重试重置——累积只反映当前尝试，与消费者看到
    的重传内容一致）。caller 不解读 state，只原样传回
    provider.snapshot_blocks(accumulator)。
    """

    __slots__ = ("state",)

    def __init__(self) -> None:
        self.state: object | None = None


class ModelProvider(ABC):
    """模型调用 provider 基类。"""

    _config: ProviderConfig
    thinking: bool = True
    reasoning_effort: str | None = None

    @property
    def name(self) -> str:
        """Provider 名称（来自 config）。"""
        return self._config.name

    @property
    def protocol(self) -> str:
        """协议类型（来自 config，如 "openai" / "anthropic"）。"""
        return self._config.protocol

    @abstractmethod
    def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
        accumulator: "StreamAccumulator | None" = None,
    ) -> AsyncIterator[LLMResponse]:
        """统一调用入口，产出 LLMResponse 流。

        accumulator：caller 持有的流累积状态容器（可选）。传入时 provider
        在每次尝试开始时填充新状态（重试重置——累积只反映当前尝试）；
        流被取消后 caller 仍可经 snapshot_blocks() 读取已累积的部分内容
        （中断补提交路径）。不传时行为与无 accumulator 完全一致。
        """
        ...  # pragma: no cover

    @abstractmethod
    async def list_models(self) -> list[str]:
        """获取可用模型列表。"""
        ...

    # ── 流累积状态（中断补提交）───────────────

    def create_accumulator(self) -> "StreamAccumulator":
        """创建 caller 持有的流累积状态容器。

        基类默认返回不透明容器（无累积语义）。具体 provider 覆盖：
        generate(..., accumulator) 每次尝试填充新状态；caller 在流被
        取消后经 snapshot_blocks() 取已累积的部分内容块。
        """
        return StreamAccumulator()

    def snapshot_blocks(self, accumulator: "StreamAccumulator | None") -> list | None:
        """从累积状态提取已生成的内容块（caller 在取消后调用）。

        语义：text/thinking 块任意长度保留；未被流协议终结的 tool 块
        丢弃（半截参数不可解析，且无配对结果会使下轮请求结构非法）。
        基类默认返回 None（无状态可取）。
        """
        return None

    async def aclose(self) -> None:
        """释放 provider 持有的资源。

        由所有者在生命周期终结时调用（agent 模板切换 / session 释放 /
        模型列表 registry 重置 / 驱逐重建）。持有长连接等资源的子类应覆盖。
        """

    def set_thinking(self, enable: bool) -> None:
        """运行时切换思考模式。子类可覆盖。

        约定：thinking 状态应从实际请求 payload 源（如 extra_body）派生，
        setter 改写同一存储——保证 property / get_status 上报 / 请求体自洽。
        """
        self.thinking = enable

    def set_reasoning_effort(self, effort: str | None) -> None:
        """运行时切换推理强度。子类可覆盖。"""
        self.reasoning_effort = effort
