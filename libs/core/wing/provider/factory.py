# wing/provider/factory.py
"""create_provider — 按 ProviderConfig.protocol 创建对应 provider 实例的工厂。"""

from __future__ import annotations

from typing import TYPE_CHECKING

from wing.provider.base import ModelProvider

if TYPE_CHECKING:
    from wing.config import ProviderConfig


def create_provider(config: ProviderConfig) -> ModelProvider:
    """根据 ProviderConfig 的 protocol 字段创建对应 provider 实例。

    实例是**无状态**的（协议 + 配置 + 连接池）；会话级参数经
    `generate(..., options=RequestOptions)` 注入。生命周期归
    ``wing.provider.pool``，调用方不持有所有权。
    """
    if config.protocol == "openai":
        from wing.provider.openai.provider import OpenAICompatProvider

        return OpenAICompatProvider(config=config)
    elif config.protocol == "anthropic":
        from wing.provider.anthropic.provider import AnthropicProvider

        return AnthropicProvider(config=config)
    else:
        raise ValueError(f"unsupported protocol: '{config.protocol}'")
