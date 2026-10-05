# wing/provider/factory.py
"""create_provider — 按 ProviderConfig.protocol 创建对应 provider 实例的工厂。"""

from __future__ import annotations

from typing import TYPE_CHECKING

from wing.provider.base import ModelProvider

if TYPE_CHECKING:
    from wing.config import ProviderConfig
    from wing.media import MediaAccess


def create_provider(
    config: ProviderConfig,
    session_id: str | None = None,
    media: MediaAccess | None = None,
) -> ModelProvider:
    """根据 ProviderConfig 的 protocol 字段创建对应 provider 实例。

    media 是会话媒体池的读写窄接口（provider 序列化图片时按 id 读字节）；
    None = 无媒体存储（registry 的仅列表 client、测试构造）。
    """
    if config.protocol == "openai":
        from wing.provider.openai.provider import OpenAICompatProvider

        return OpenAICompatProvider(config=config, session_id=session_id, media=media)
    elif config.protocol == "anthropic":
        from wing.provider.anthropic.provider import AnthropicProvider

        return AnthropicProvider(config=config, session_id=session_id, media=media)
    else:
        raise ValueError(f"unsupported protocol: '{config.protocol}'")
