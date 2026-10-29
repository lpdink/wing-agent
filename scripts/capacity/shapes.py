#!/usr/bin/env python3
"""负载形态注册表：``--scenario`` 的唯一取值来源。

``run.py``（注册剧本 / 校验参数）与 ``driver_shard.py``（发 prompt）都经
:func:`resolve` 拿同一个模块对象，因此形态的「剧本」与「prompt 文本」只有一处定义
——否则配对键会在两侧漂移，延迟样本会静默变成全 ``dropped``。

新增形态：写一个满足 ``shape_pure_stream`` 契约（``NAME`` / ``MODEL`` /
``script`` / ``prompt_text``）的模块，在 :data:`SHAPES` 里登记一行。
"""

from __future__ import annotations

import importlib
from types import ModuleType

SHAPES: dict[str, str] = {
    "pure_stream": "shape_pure_stream",
}
"""``--scenario`` → 模块名（本目录内的同级模块）。"""

_REQUIRED = ("NAME", "MODEL", "script", "prompt_text")


class ShapeError(ValueError):
    """未知形态，或形态模块没有满足契约。"""


def names() -> list[str]:
    return sorted(SHAPES)


def resolve(name: str) -> ModuleType:
    """按名字取形态模块（未知名 / 契约不全 → ``ShapeError``，附可选项）。"""
    module_name = SHAPES.get(name)
    if module_name is None:
        raise ShapeError(
            f"unknown scenario {name!r}; available: {', '.join(names()) or '<none>'}"
        )
    module = importlib.import_module(module_name)
    missing = [attr for attr in _REQUIRED if not hasattr(module, attr)]
    if missing:
        raise ShapeError(
            f"scenario module {module_name!r} does not implement the shape "
            f"contract: missing {', '.join(missing)}"
        )
    if module.NAME != name:
        raise ShapeError(
            f"scenario {name!r} is registered to module {module_name!r}, but the "
            f"module declares NAME={module.NAME!r}"
        )
    return module
