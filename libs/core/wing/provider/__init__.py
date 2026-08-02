# wing/provider/__init__.py
"""模型调用 provider 层。

提供 ModelProvider 基类、工厂函数 create_provider()，以及模块级 provider
registry——创建并持有所有 provider 的 client，对外提供聚合模型列表
（配置了静态 models 的 provider 跳过请求；并发查询、单个失败落空）。
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.provider.base import ModelProvider

if TYPE_CHECKING:
    from wing.config import ProviderConfig

__all__ = [
    "ModelProvider",
    "ProviderModels",
    "create_provider",
    "list_all_models",
    "reset_registry",
]

_LIST_MODELS_TIMEOUT = 10.0


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


@dataclass
class ProviderModels:
    """按 provider 聚合的模型列表（嵌套响应条目）。"""

    provider: str
    models: list[str] = field(default_factory=list)


class _ProviderRegistry:
    """模块级 provider client registry——仅服务模型列表聚合查询。

    client 长持有（消灭每请求临时创建的连接泄漏）。与 session 调用 client
    （WingAgent 按 agent 持有的表）是两批实例：registry client 只做
    list_models，无每-session 可变状态，共享无串味。
    """

    def __init__(self) -> None:
        self._providers: dict[str, ModelProvider] = {}

    def _ensure(self) -> None:
        """按当前 config 懒创建（已有同名条目跳过）。"""
        from wing.config import get_config

        for cfg in get_config().providers:
            if cfg.name not in self._providers:
                self._providers[cfg.name] = create_provider(cfg)

    async def list_all_models(self) -> list[ProviderModels]:
        """并发查询所有 provider（per-provider 超时；失败落空不影响其余）。"""
        self._ensure()

        async def _query_one(name: str, provider: ModelProvider) -> ProviderModels:
            try:
                models = await asyncio.wait_for(
                    provider.list_models(), timeout=_LIST_MODELS_TIMEOUT
                )
                return ProviderModels(provider=name, models=models)
            except Exception as e:
                log.error(f"list_models failed for provider '{name}': {e}")
                return ProviderModels(provider=name, models=[])

        return list(
            await asyncio.gather(
                *[_query_one(n, p) for n, p in self._providers.items()]
            )
        )

    async def reset(self) -> None:
        """关闭全部 client 并清表（config reload 时调用；下次查询按新配置重建）。"""
        providers = list(self._providers.values())
        self._providers.clear()
        for provider in providers:
            await provider.aclose()


_registry = _ProviderRegistry()


async def list_all_models() -> list[ProviderModels]:
    """聚合模型列表（按 provider 分组；配置了静态 models 的跳过请求）。"""
    return await _registry.list_all_models()


async def reset_registry() -> None:
    """关闭并清空 registry（config reload 入口）。"""
    await _registry.reset()
