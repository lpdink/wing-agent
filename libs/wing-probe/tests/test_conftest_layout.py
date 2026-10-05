"""位置门禁：必须在 xdist controller 上生效的钩子只能住 ``libs/wing-probe/conftest.py``。

**为什么需要**（实测机制，见 `docs/dev/probe-testing.md` 的「并行」一节）：
pytest 的 conftest 加载只从某个路径**向上**走（`Config._importconftest`：
`for parent in reversed((directory, *directory.parents))`），而从不下潜。
执行测试的进程在收集时会逐层加载，但 **xdist 的 controller 不做收集**——它只为
`args` 调这个函数。所以 `pytest libs/wing-probe/`（`make test-probe` 的原样调用）
时 controller 只加载 args 那一层的 conftest；嵌套的 `scenarios/conftest.py` 它
永远看不见。

失效形态是**静默**的：把 artifacts 台账的合并/汇总钩子放进嵌套层，串行照常工作，
一开 `-n` 就变成死代码——失败现场的转储路径与逃生舱理由整体消失（CI 上那是唯一
的现场），而测试仍然全绿。所以这条约定要有门禁，不能靠记性。

门禁走 AST：改名的同义本地函数（`def _merge(...)`）不算数，钩子名是协议的一部分。
"""

from __future__ import annotations

import ast
from pathlib import Path

from wing_probe.guard import PKG_ROOT

#: 必须由 controller 与 worker **都**加载到的那几个钩子（artifacts 台账的生命周期）。
LEDGER_HOOKS = frozenset(
    {"pytest_sessionfinish", "pytest_testnodedown", "pytest_terminal_summary"}
)

#: args 那一层的 conftest（`make test-probe` / 单场景 / 单文件三种形态的公共祖先）。
ROOT_CONFTEST = PKG_ROOT / "conftest.py"


def hook_names(path: Path) -> set[str]:
    """该文件定义的 pytest 钩子名（AST 级，不匹配字符串/注释）。"""
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    return {
        node.name
        for node in ast.walk(tree)
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
        and node.name.startswith("pytest_")
    }


def nested_conftests() -> list[Path]:
    """args 层以下的 conftest（收集期才被加载的那些）。"""
    return sorted(
        path
        for path in PKG_ROOT.rglob("conftest.py")
        if path != ROOT_CONFTEST and ".venv" not in path.parts
    )


def test_ledger_hooks_live_at_the_args_level() -> None:
    """台账钩子在 args 那一层（controller 一定加载得到）。"""
    assert hook_names(ROOT_CONFTEST) >= LEDGER_HOOKS, (
        f"{ROOT_CONFTEST} 必须定义 {sorted(LEDGER_HOOKS)}——"
        "这些钩子要在 xdist 的 controller 上生效，而 controller 不加载嵌套 conftest"
    )


def test_nested_conftests_do_not_redefine_ledger_hooks() -> None:
    """嵌套 conftest 不得再定义它们：那样只会让人以为并行下也生效。"""
    offenders = {
        str(path.relative_to(PKG_ROOT)): sorted(hook_names(path) & LEDGER_HOOKS)
        for path in nested_conftests()
        if hook_names(path) & LEDGER_HOOKS
    }
    assert not offenders, (
        "嵌套 conftest 里的台账钩子不会被 controller 加载（并行下静默失效）：\n"
        f"{offenders}"
    )


def test_rootdir_is_pinned_to_the_args_level() -> None:
    """前提自证：rootdir / confcutdir 就钉在这一层。

    `_importconftest` 只在 confcutdir 之内向上找 conftest——若 `libs/wing-probe/
    pyproject.toml` 的 `[tool.pytest.ini_options]` 被挪走，rootdir 会变成仓库根，
    本层 conftest 对 `pytest libs/wing-probe/` 之外的其他参数形态就可能失效。
    """
    ini = PKG_ROOT / "pyproject.toml"
    assert ini.is_file(), f"{ini} 是 rootdir 的锚点（[tool.pytest.ini_options]）"
    assert "[tool.pytest.ini_options]" in ini.read_text(encoding="utf-8")
