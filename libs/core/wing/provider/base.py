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
    ) -> AsyncIterator[LLMResponse]:
        """统一调用入口，产出 LLMResponse 流。"""
        ...  # pragma: no cover

    @abstractmethod
    async def list_models(self) -> list[str]:
        """获取可用模型列表。"""
        ...

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
