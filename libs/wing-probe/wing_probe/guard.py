"""import 门禁 —— 禁止 probe 代码 import 产品运行时（``wing`` / ``wing.*``）。

为什么需要：probe 的价值在于"站在系统外面"——只经公开 HTTP / WS 协议压后端。
一旦 probe 里出现 ``import wing``，probe 就与实现共享了内部结构，"两边一起改、
漂移抓不到"的测试退化会重新出现，而且这类退化是静默的（测试照常绿）。

本模块用 AST 扫描（不是正则、不是字符串匹配）检出四种导入形态：

- ``import wing`` / ``import wing.event``
- ``from wing.event import …`` / ``from wing import …``
- ``importlib.import_module("wing.runtime")``（含 ``import_module("wing")``）
- ``__import__("wing.runtime")``

允许清单：``wing_probe``（自身）、``wing_sdk``（面向外部宿主的独立包，
probe driver 的合法入口）与一切第三方包。禁用面是"模块名恰为 ``wing``
或以 ``wing.`` 开头"，因此 ``wing_probe`` / ``wing_sdk`` 天然不被误伤。

门禁在每次测试运行中自证：``tests/test_no_wing_imports.py`` 扫描
``libs/wing-probe/`` 下全部 Python 源码（``wing_probe/`` / ``scenarios/`` /
``tests/``），任何违规即红。
"""

from __future__ import annotations

import ast
from collections.abc import Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path

#: 被禁模块的根名（恰为它或以 ``它.`` 开头即违规）。
FORBIDDEN_ROOT = "wing"

#: 允许的 wing 系模块（显式列出，供报告与自测引用）。
ALLOWED_MODULES: tuple[str, ...] = ("wing_probe", "wing_sdk")

#: 扫描目录时跳过的目录名（缓存 / 虚拟环境 / 依赖）。
EXCLUDED_DIRS: frozenset[str] = frozenset(
    {
        ".git",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        ".ty_cache",
        ".venv",
        "__pycache__",
        "node_modules",
    }
)

#: 本包根目录（``libs/wing-probe/``）；仓库内测试用它作扫描根。
PKG_ROOT: Path = Path(__file__).resolve().parent.parent

#: 检出 JSON 字符串形式导入的函数名（``importlib.import_module`` 等）。
_STRING_IMPORT_CALLS: frozenset[str] = frozenset(
    {
        "import_module",
        "importlib.import_module",
        "__import__",
    }
)


@dataclass(frozen=True)
class Violation:
    """一处违规导入。``kind`` 取值见模块级 ``KIND_*`` 常量。"""

    path: str
    lineno: int
    col_offset: int
    kind: str
    target: str
    line: str = ""

    def __str__(self) -> str:
        where = f"{self.path}:{self.lineno}:{self.col_offset}"
        detail = f" → {self.line.strip()}" if self.line.strip() else ""
        return f"{where}: forbidden import ({self.kind}) of '{self.target}'{detail}"


KIND_IMPORT = "import"
KIND_FROM_IMPORT = "from-import"
KIND_IMPORT_MODULE = "importlib.import_module"
KIND_DUNDER_IMPORT = "__import__"


def is_forbidden(module: str) -> bool:
    """``wing`` 与 ``wing.*`` 违规；``wing_probe`` / ``wing_sdk`` 不违规。"""
    return module == FORBIDDEN_ROOT or module.startswith(f"{FORBIDDEN_ROOT}.")


def _static_str(node: ast.AST) -> str | None:
    """常量字符串（含纯字面量拼接），无法静态求值返回 None。"""
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        left = _static_str(node.left)
        right = _static_str(node.right)
        if left is not None and right is not None:
            return left + right
    return None


def _call_name(node: ast.Call) -> str | None:
    """被调用者的点分名（``import_module`` / ``importlib.import_module``）。"""
    func = node.func
    if isinstance(func, ast.Name):
        return func.id
    if isinstance(func, ast.Attribute):
        base = func.value
        if isinstance(base, ast.Name):
            return f"{base.id}.{func.attr}"
        if isinstance(base, ast.Attribute) and isinstance(base.value, ast.Name):
            return f"{base.value.id}.{base.attr}.{func.attr}"
    return None


def _call_string_arg(node: ast.Call) -> str | None:
    """字符串形式导入的第一个参数（位置参数或 ``name=``）。"""
    if node.args:
        return _static_str(node.args[0])
    for keyword in node.keywords:
        if keyword.arg in (None, "name"):
            return _static_str(keyword.value)
    return None


def scan_source(source: str, *, path: str = "<string>") -> list[Violation]:
    """扫描一段源码，返回全部违规（按行号排序）。

    语法错误不做处理（返回空列表）——那是编译期 / ruff 的职责，门禁只认
    可解析代码里的导入形态。
    """
    try:
        tree = ast.parse(source, filename=path)
    except SyntaxError:
        return []

    lines = source.splitlines()
    found: list[Violation] = []

    def record(node: ast.AST, kind: str, target: str) -> None:
        lineno = getattr(node, "lineno", 1)
        col = getattr(node, "col_offset", 0)
        text = lines[lineno - 1] if 0 < lineno <= len(lines) else ""
        found.append(Violation(path, lineno, col, kind, target, text))

    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if is_forbidden(alias.name):
                    record(node, KIND_IMPORT, alias.name)
        elif isinstance(node, ast.ImportFrom):
            # level > 0 是相对导入（不可能是 wing 产品包）。
            if node.level == 0 and node.module and is_forbidden(node.module):
                record(node, KIND_FROM_IMPORT, node.module)
        elif isinstance(node, ast.Call):
            name = _call_name(node)
            if name not in _STRING_IMPORT_CALLS:
                continue
            target = _call_string_arg(node)
            if target is not None and is_forbidden(target):
                kind = (
                    KIND_DUNDER_IMPORT if name == "__import__" else KIND_IMPORT_MODULE
                )
                record(node, kind, target)

    found.sort(key=lambda v: (v.lineno, v.col_offset))
    return found


def scan_file(path: str | Path) -> list[Violation]:
    """扫描单个文件（读取失败按空处理——门禁不掩盖 I/O 错误另有测试兜底）。"""
    file_path = Path(path)
    try:
        source = file_path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return []
    return scan_source(source, path=str(file_path))


def iter_python_files(
    root: str | Path,
    *,
    excluded_dirs: frozenset[str] = EXCLUDED_DIRS,
) -> Iterator[Path]:
    """遍历源码树下的全部 ``*.py``（跳过缓存 / 虚拟环境目录），按路径排序。"""
    base = Path(root)
    files: list[Path] = []
    for path in base.rglob("*.py"):
        if any(part in excluded_dirs for part in path.relative_to(base).parts):
            continue
        if path.is_file():
            files.append(path)
    yield from sorted(files)


def scan_tree(
    root: str | Path,
    *,
    excluded_dirs: frozenset[str] = EXCLUDED_DIRS,
) -> list[Violation]:
    """扫描整个源码树（``wing_probe/`` + ``scenarios/`` + ``tests/``）。"""
    violations: list[Violation] = []
    for path in iter_python_files(root, excluded_dirs=excluded_dirs):
        violations.extend(scan_file(path))
    return violations


def format_violations(violations: Sequence[Violation]) -> str:
    """违规清单的可读报告（空清单给出"无违规"文案）。"""
    if not violations:
        return "no forbidden imports found"
    header = (
        f"{len(violations)} forbidden import(s) "
        f"(probe code must not import '{FORBIDDEN_ROOT}'; "
        f"allowed: {', '.join(ALLOWED_MODULES)}):"
    )
    return "\n".join([header, *(f"  {v}" for v in violations)])


def check_tree(root: str | Path) -> str:
    """扫描 + 渲染报告：便于测试 / 脚本一行拿到可读结论。"""
    return format_violations(scan_tree(root))


__all__ = [
    "ALLOWED_MODULES",
    "EXCLUDED_DIRS",
    "FORBIDDEN_ROOT",
    "KIND_DUNDER_IMPORT",
    "KIND_FROM_IMPORT",
    "KIND_IMPORT",
    "KIND_IMPORT_MODULE",
    "PKG_ROOT",
    "Violation",
    "check_tree",
    "format_violations",
    "is_forbidden",
    "iter_python_files",
    "scan_file",
    "scan_source",
    "scan_tree",
]
