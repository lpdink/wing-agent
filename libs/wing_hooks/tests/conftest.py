"""wing_hooks 测试的路径垫片。

``libs/wing_hooks`` 不在 uv workspace 成员里（members 只有 core / wing-sdk /
wing-probe / crates），仓库 venv 里没有 ``wing_hooks`` 这个顶层包——不垫
``sys.path`` 时 ``uv run pytest libs/wing_hooks/tests/`` 会在收集期
``ModuleNotFoundError``。把包根（``libs/wing_hooks``）插进 ``sys.path``，
``test_*.py`` 里的 ``from wing_hooks.xxx import ...`` 才成立。
"""

import sys
from pathlib import Path

_PACKAGE_ROOT = str(Path(__file__).resolve().parents[1])
if _PACKAGE_ROOT not in sys.path:
    sys.path.insert(0, _PACKAGE_ROOT)
