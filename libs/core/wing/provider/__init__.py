# wing/provider/__init__.py
"""模型调用 provider 层。

包入口只放稳定入口（不承载服务实例）：

  - ``ModelProvider``：provider 基类（无状态契约；协议实现见 ``openai`` /
    ``anthropic`` 子包）
  - ``RequestOptions``：会话级调用参数（provider 无状态化的唯一注入点）
  - ``create_provider()``：按协议创建实例的工厂（``factory`` 模块）

共享 provider 池（全部会话与模型列表共用同一批实例，生命周期归它）住在
``wing.provider.pool``——那是服务实例的家，不是包入口的。
"""

from __future__ import annotations

from wing.provider.base import ModelProvider, RequestOptions
from wing.provider.factory import create_provider

__all__ = [
    "ModelProvider",
    "RequestOptions",
    "create_provider",
]
