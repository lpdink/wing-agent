# wing/provider/base.py
"""ModelProvider ABC — 模型调用层的统一接口。

所有协议实现（OpenAI 兼容、Anthropic）继承此基类，
输出统一的 LLMResponse 流，上层 ReActLoop 对协议无感知。
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import TYPE_CHECKING, AsyncIterator

from wing.schema import LLMResponse, Message, Tool

if TYPE_CHECKING:
    from wing.config import ProviderConfig


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
