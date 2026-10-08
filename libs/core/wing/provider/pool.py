# wing/provider/pool.py
"""共享 provider 池 — 全部会话与模型列表共用同一批无状态 provider 实例。

为什么共享（相对旧版「每 agent 一份 client 表」）：

- 每份 client 自带一个 httpx 连接池。连接一旦建过，keepalive socket 会一直
  挂着（httpcore 的过期检查只在下次使用连接池时发生）——会话数 × provider
  数直接放大进程 FD 占用（macOS 默认 ulimit 仅 256）；
- reload / 模板切换的每次重建都会制造一批新 client：旧写法「先建后关」还会
  把在途请求的 client 关死，重试栈从此没有一次 attempt 可能成功（#172）。

池的语义：

- 每 provider name 一个实例，懒创建（首个 get 时按当前配置构建）；
- 会话侧只持有 name（`WingAgent.provider_name`），实例经 `get_provider()`
  实时解析——reload 换新后新请求立即用新配置，无需逐会话重建；
- ``reset_providers()``（reload 入口）按新配置**先建后换**：任一 provider
  构建失败时整个池保持原样；被替换的旧实例 `retire()`——在途请求不受影响，
  收尾后自动关闭；
- 配置中已移除的 name：池保留旧实例（钉在它上面的会话继续可用），新解析
  按当前配置——与旧版「活跃 provider 重建失败时保持可用」的会话不拆解
  语义一致；
- ``/api/models`` 聚合复用同一批实例（不再维护第二套只读 client）。
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.config import ModelCapabilities
from wing.provider.base import ModelProvider
from wing.provider.factory import create_provider

if TYPE_CHECKING:
    from wing.config import ProviderConfig

__all__ = [
    "ModelDetail",
    "ProviderModels",
    "ProviderPool",
    "close_providers",
    "get_provider",
    "list_all_models",
    "reset_providers",
]

_LIST_MODELS_TIMEOUT = 10.0


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


class ProviderPool:
    """provider name → 共享实例；实例的生命周期（构建 / 退场 / 关闭）全归本池。"""

    def __init__(self) -> None:
        self._providers: dict[str, ModelProvider] = {}
        self._configs: dict[str, ProviderConfig] = {}
        """实例建表时的配置快照（存活期内不变；聚合查询据此解析模型声明）。"""

    # ── 解析 ──────────────────────────────────────

    def get(self, name: str) -> ModelProvider:
        """按 name 取共享实例；缺失时按当前配置懒创建。

        配置中已移除的 name 保留旧实例（钉住它的会话不因 reload 被拆）；
        池中也没有则按当前配置解析并如实报错。
        """
        provider = self._providers.get(name)
        if provider is not None:
            return provider

        from wing.config import get_config

        cfg = get_config().get_provider(name)  # 名称未知 → 按配置报错
        provider = create_provider(cfg)
        self._providers[name] = provider
        self._configs[name] = cfg
        return provider

    # ── 生命周期 ──────────────────────────────────

    async def reset(self) -> int:
        """按当前配置重建全部在场 provider（reload 入口）；返回重建数。

        先建后换：任一构建失败（配置损坏 / 协议不支持）整个池保持原样，
        返回给 reload 的逐项 detail 如实失败；被替换的旧实例 `retire()`，
        在途请求跑完后自动关闭。
        """
        from wing.config import get_config

        configs = list(get_config().providers)
        fresh: dict[str, ModelProvider] = {}
        for cfg in configs:
            fresh[cfg.name] = create_provider(cfg)

        displaced = [
            provider for name, provider in self._providers.items() if name in fresh
        ]
        self._providers.update(fresh)
        self._configs.update({cfg.name: cfg for cfg in configs})
        for provider in displaced:
            await provider.retire()
        log.info(
            f"provider pool reset: rebuilt {len(fresh)} provider(s), "
            f"retired {len(displaced)} old instance(s)"
        )
        return len(fresh)

    async def close(self) -> None:
        """关闭并清空池（进程终结 / 测试；在途请求随之终止）。"""
        providers = list(self._providers.values())
        self._providers.clear()
        self._configs.clear()
        for provider in providers:
            await provider.aclose()

    # ── 模型列表聚合 ──────────────────────────────

    async def list_all_models(self) -> list[ProviderModels]:
        """并发查询当前配置声明的全部 provider（per-provider 超时；失败落空）。"""
        from wing.config import get_config

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

        configs = list(get_config().providers)
        return list(
            await asyncio.gather(
                *(_query_one(cfg.name, cfg, self.get(cfg.name)) for cfg in configs)
            )
        )


_pool = ProviderPool()


def get_provider(name: str) -> ModelProvider:
    """按 name 取共享 provider 实例（懒创建；见 `ProviderPool.get`）。"""
    return _pool.get(name)


async def reset_providers() -> int:
    """按当前配置重建共享 provider 池（config reload 入口）；返回重建数。"""
    return await _pool.reset()


async def close_providers() -> None:
    """关闭并清空共享 provider 池（进程终结入口）。"""
    await _pool.close()


async def list_all_models() -> list[ProviderModels]:
    """聚合模型列表（按 provider 分组；配置了静态 models 的跳过请求）。"""
    return await _pool.list_all_models()
