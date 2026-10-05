# wing/hooks/__init__.py
"""wing/hooks 包 — Hook 扩展点：注册表 + 配置文件加载。

11 归位：顶层 ``hook_registry.py`` 与 ``config.load_hooks``（加载器）合成一个包——
注册表与它的加载器住一处。符号名不变，公共入口经包根 re-export：

    from wing.hooks import HookRegistry, hooks, load_hooks

内部模块：``registry``（HookRegistry 管道 + 全局单例 hooks）·
``loader``（glob 匹配 .py 文件并 import，注册经 ``hooks.on()`` 装饰器发生）。
"""

from .loader import load_hooks
from .registry import HookRegistry, hooks

__all__ = [
    "HookRegistry",
    "hooks",
    "load_hooks",
]
