# wing/provider/__init__.py
"""模型调用 provider 层。

包入口只放稳定入口（不承载服务实例）：

  - ``ModelProvider``：provider 基类（协议实现见 ``openai_compat`` / ``anthropic``）
  - ``create_provider()``：按协议创建实例的工厂（``factory`` 模块）

模块级 provider client registry（聚合模型列表，服务 ``/api/models``）住在
``wing.provider.registry``——那是服务实例的家，不是包入口的。
"""

from __future__ import annotations

from wing.provider.base import ModelProvider
from wing.provider.factory import create_provider

__all__ = [
    "ModelProvider",
    "create_provider",
]
