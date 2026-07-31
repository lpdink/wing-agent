# wing/provider/base.py
"""ModelProvider ABC — 模型调用层的统一接口。

所有协议实现（OpenAI 兼容、Anthropic）继承此基类，
输出统一的 LLMResponse 流，上层 LLMCaller 对协议无感知。
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import AsyncIterator

from wing.schema import LLMResponse, Message, Tool


class ModelProvider(ABC):
    """模型调用 provider 基类。"""

    thinking: bool = True
    reasoning_effort: str | None = None

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

    def set_thinking(self, enable: bool) -> None:
        """运行时切换思考模式。子类可覆盖。"""
        self.thinking = enable

    def set_reasoning_effort(self, effort: str | None) -> None:
        """运行时切换推理强度。子类可覆盖。"""
        self.reasoning_effort = effort

    def reload(self) -> list[str]:
        """从配置热刷新，返回变更项列表。子类可覆盖。"""
        return []
