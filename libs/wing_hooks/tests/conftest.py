"""wing_hooks 测试的路径垫片 + 全局 HookRegistry 隔离。

**路径垫片**：``libs/wing_hooks`` 不在 uv workspace 成员里（members 只有 core /
wing-sdk / wing-probe / crates），仓库 venv 里没有 ``wing_hooks`` 这个顶层包——
不垫 ``sys.path`` 时 ``uv run pytest libs/wing_hooks/tests/`` 会在收集期
``ModuleNotFoundError``。把包根（``libs/wing_hooks``）插进 ``sys.path``，
``test_*.py`` 里的 ``from wing_hooks.xxx import ...`` 才成立。

**全局 HookRegistry 隔离**：wing_hooks 的模块按包约定在 **import 期**就把 handler
注册进全局 ``hooks`` 单例（``@hooks.on(...)`` 装饰器），测试文件 import 它们会污染
同一个 pytest session 里的其它测试——典型是 core 的 ``before_user_message`` 断言
（``uv run pytest libs/wing_hooks/tests/ libs/core/tests/`` 会红）；反方向也有一条
（core 的 fixture 会 ``hooks.clear()``，把 wing_hooks 的登记清掉，让依赖"import 期
注册"的断言变红）。这里在 conftest 装载时刻——**早于**同目录测试模块被 import——
对全局注册表做快照，并在每个测试前后恢复：wing_hooks 的注册不再泄漏到 session 的
其它部分，且各测试都从同一份干净现场出发。
"""

import sys
from pathlib import Path
from typing import Any, Iterator

import pytest

_PACKAGE_ROOT = str(Path(__file__).resolve().parents[1])
if _PACKAGE_ROOT not in sys.path:
    sys.path.insert(0, _PACKAGE_ROOT)

from wing.hooks import hooks as _global_hooks  # noqa: E402  （必须在路径垫片之后）

#: conftest 装载时刻的全局注册表快照（先于同目录测试模块的 import 期注册）。
#: 注：HookRegistry 没有公开的枚举/快照 API，测试垫片这里直接读内部结构
#: （``_handlers``：point → handler 列表）；注册表若将来提供公开快照口，换掉即可。
_GLOBAL_HOOKS_SNAPSHOT: dict[str, list[Any]] = {
    point: list(handlers) for point, handlers in _global_hooks._handlers.items()
}


def _restore_global_hooks() -> None:
    """把全局注册表恢复成快照（原地清空 + 回填，保持 defaultdict 语义）。"""
    registry = _global_hooks._handlers
    registry.clear()
    for point, handlers in _GLOBAL_HOOKS_SNAPSHOT.items():
        registry[point] = list(handlers)


@pytest.fixture(autouse=True)
def _isolate_global_hooks() -> Iterator[None]:
    """每个测试前后把全局注册表恢复成 conftest 装载时刻的样子（理由见模块 docstring）。"""
    _restore_global_hooks()
    yield
    _restore_global_hooks()
