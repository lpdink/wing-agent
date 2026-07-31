# wing/provider/__init__.py
"""模型调用 provider 层。

提供 ModelProvider 基类和工厂函数 create_provider()。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from wing.provider.base import ModelProvider

if TYPE_CHECKING:
    from wing.config import ProviderConfig

__all__ = ["ModelProvider", "create_provider"]


def create_provider(
    config: ProviderConfig,
    session_id: str | None = None,
) -> ModelProvider:
    """根据 ProviderConfig 的 protocol 字段创建对应 provider 实例。"""
    if config.protocol == "openai":
        from wing.provider.openai_compat import OpenAICompatProvider

        return OpenAICompatProvider(config=config, session_id=session_id)
    elif config.protocol == "anthropic":
        from wing.provider.anthropic import AnthropicProvider

        return AnthropicProvider(config=config, session_id=session_id)
    else:
        raise ValueError(f"unsupported protocol: '{config.protocol}'")
