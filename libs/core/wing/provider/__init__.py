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
from wing.config import ModelCapabilities
from wing.provider.base import ModelProvider

if TYPE_CHECKING:
    from wing.config import ProviderConfig

__all__ = [
    "ModelDetail",
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
class ModelDetail:
    """单条模型声明的对外投影（与 ``ProviderModels.models`` 逐项同序对应）。

    ``capabilities`` 直接复用 config 的声明类型（能力词汇只有一份）。
    """

    name: str
    """实际调用名。"""
    display_name: str | None = None
    """展示名（缺省由前端回落 name）。"""
    description: str | None = None
    capabilities: ModelCapabilities = field(default_factory=ModelCapabilities)


@dataclass
class ProviderModels:
    """按 provider 聚合的模型列表（嵌套响应条目）。"""

    provider: str
    models: list[str] = field(default_factory=list)
    model_details: list[ModelDetail] = field(default_factory=list)
    """与 models 逐项同序同名：配置声明的带元信息，远端发现的最小化。"""


def _model_detail(cfg: ProviderConfig, model: str) -> ModelDetail:
    """按声明构建 detail：已声明带元信息，未声明（远端发现）给最小条目。"""
    spec = cfg.find_model(model)
    if spec is None:
        return ModelDetail(name=model)
    return ModelDetail(
        name=spec.name,
        display_name=spec.display_name,
        description=spec.description,
        capabilities=spec.capabilities,
    )


class _ProviderRegistry:
    """模块级 provider client registry——仅服务模型列表聚合查询。

    client 长持有（消灭每请求临时创建的连接泄漏）。与 session 调用 client
    （WingAgent 按 agent 持有的表）是两批实例：registry client 只做
    list_models，无每-session 可变状态，共享无串味。
    """

    def __init__(self) -> None:
        self._providers: dict[str, ModelProvider] = {}
        self._configs: dict[str, ProviderConfig] = {}
        """client 建表时对应的配置快照——聚合时据此解析模型声明明细。"""

    def _ensure(self) -> None:
        """按当前 config 懒创建（已有同名条目跳过；config reload 走 reset 重建）。"""
        from wing.config import get_config

        for cfg in get_config().providers:
            if cfg.name not in self._providers:
                self._providers[cfg.name] = create_provider(cfg)
                self._configs[cfg.name] = cfg

    async def list_all_models(self) -> list[ProviderModels]:
        """并发查询所有 provider（per-provider 超时；失败落空不影响其余）。"""
        self._ensure()

        async def _query_one(
            name: str, cfg: ProviderConfig, provider: ModelProvider
        ) -> ProviderModels:
            try:
                models = await asyncio.wait_for(
                    provider.list_models(), timeout=_LIST_MODELS_TIMEOUT
                )
                return ProviderModels(
                    provider=name,
                    models=models,
                    model_details=[_model_detail(cfg, m) for m in models],
                )
            except Exception as e:
                log.error(f"list_models failed for provider '{name}': {e}")
                return ProviderModels(provider=name, models=[])

        return list(
            await asyncio.gather(
                *[
                    _query_one(n, self._configs[n], p)
                    for n, p in self._providers.items()
                ]
            )
        )

    async def reset(self) -> None:
        """关闭全部 client 并清表（config reload 时调用；下次查询按新配置重建）。"""
        providers = list(self._providers.values())
        self._providers.clear()
        self._configs.clear()
        for provider in providers:
            await provider.aclose()


_registry = _ProviderRegistry()


async def list_all_models() -> list[ProviderModels]:
    """聚合模型列表（按 provider 分组；配置了静态 models 的跳过请求）。"""
    return await _registry.list_all_models()


async def reset_registry() -> None:
    """关闭并清空 registry（config reload 入口）。"""
    await _registry.reset()
