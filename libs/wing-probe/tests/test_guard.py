"""门禁扫描器单测：四种违规形态必检出，允许清单不误伤。

覆盖 tasks 1.5。全部为纯单测：扫描源码字符串 / 临时目录，不起任何进程。
"""

from pathlib import Path

from wing_probe.guard import (
    KIND_DUNDER_IMPORT,
    KIND_FROM_IMPORT,
    KIND_IMPORT,
    KIND_IMPORT_MODULE,
    format_violations,
    is_forbidden,
    scan_file,
    scan_source,
    scan_tree,
)


def _kinds(source: str) -> list[str]:
    return [v.kind for v in scan_source(source)]


def test_plain_import_detected() -> None:
    violations = scan_source("import wing\n")
    assert len(violations) == 1
    assert violations[0].kind == KIND_IMPORT
    assert violations[0].target == "wing"
    assert violations[0].lineno == 1


def test_dotted_import_detected() -> None:
    violations = scan_source("x = 1\nimport wing.event as ev\n")
    assert [v.kind for v in violations] == [KIND_IMPORT]
    assert violations[0].target == "wing.event"
    assert violations[0].lineno == 2


def test_from_import_detected() -> None:
    violations = scan_source("from wing.event import TextEvent\n")
    assert _kinds("from wing.event import TextEvent\n") == [KIND_FROM_IMPORT]
    assert violations[0].target == "wing.event"


def test_from_bare_package_import_detected() -> None:
    violations = scan_source("from wing import runtime\n")
    assert [v.target for v in violations] == ["wing"]


def test_import_module_detected() -> None:
    violations = scan_source(
        'import importlib\nm = importlib.import_module("wing.runtime")\n'
    )
    assert [v.kind for v in violations] == [KIND_IMPORT_MODULE]
    assert violations[0].target == "wing.runtime"
    assert violations[0].lineno == 2


def test_import_module_aliased_forms_detected() -> None:
    assert _kinds('from importlib import import_module\nimport_module("wing")\n') == [
        KIND_IMPORT_MODULE
    ]
    assert _kinds('importlib.import_module(name="wing.tools")\n') == [
        KIND_IMPORT_MODULE
    ]


def test_dunder_import_detected() -> None:
    violations = scan_source('mod = __import__("wing.schema")\n')
    assert [v.kind for v in violations] == [KIND_DUNDER_IMPORT]
    assert violations[0].target == "wing.schema"


def test_literal_concatenation_detected() -> None:
    violations = scan_source('importlib.import_module("wing" + ".runtime")\n')
    assert [v.target for v in violations] == ["wing.runtime"]


def test_allowed_modules_pass() -> None:
    source = "\n".join(
        [
            "import wing_sdk",
            "import wing_probe",
            "import wing_probe.provider",
            "from wing_sdk import ToolHost",
            "from wing_probe.guard import scan_tree",
            "from . import sibling",
            "from .probe import thing",
            "import aiohttp, httpx, yaml  # noqa",
            'importlib.import_module("wing_sdk.host")',
            'importlib.import_module("wing_probe.provider.server")',
            'mod = __import__("wing_sdk")',
            "import os.path",
            "import wingspan",  # 仅前缀相同，不是 wing 包
        ]
    )
    assert scan_source(source) == []


def test_dynamic_name_not_flagged() -> None:
    # 运行时字符串（非常量）不参与静态判定——门禁不猜。
    assert scan_source("importlib.import_module(name)\n") == []
    assert scan_source('importlib.import_module(prefix + ".x")\n') == []


def test_syntax_error_does_not_crash() -> None:
    assert scan_source("def broken(:\n") == []


def test_is_forbidden_boundaries() -> None:
    assert is_forbidden("wing")
    assert is_forbidden("wing.gateway.routes")
    assert not is_forbidden("wing_probe")
    assert not is_forbidden("wing_sdk")
    assert not is_forbidden("wingspan")
    assert not is_forbidden("mywing")


def test_line_numbers_and_column_reported() -> None:
    source = "\n".join(
        [
            "import os",
            "",
            "if True:",
            "    import wing",
            "",
            "def f():",
            "    return __import__('wing.runtime')",
        ]
    )
    violations = scan_source(source)
    assert [(v.lineno, v.col_offset) for v in violations] == [(4, 4), (7, 11)]
    assert "import wing" in str(violations[0])


def test_scan_file_and_format_report(tmp_path: Path) -> None:
    bad = tmp_path / "bad.py"
    bad.write_text("import wing\n", encoding="utf-8")
    good = tmp_path / "good.py"
    good.write_text("import wing_sdk\n", encoding="utf-8")

    violations = scan_file(bad)
    assert len(violations) == 1
    report = format_violations(violations)
    assert str(bad) in report
    assert "1 forbidden import(s)" in report

    assert scan_file(good) == []
    assert format_violations([]) == "no forbidden imports found"


def test_scan_tree_skips_caches_and_sorts(tmp_path: Path) -> None:
    (tmp_path / "wing_probe").mkdir()
    (tmp_path / "wing_probe" / "b.py").write_text("import wing\n", encoding="utf-8")
    (tmp_path / "wing_probe" / "a.py").write_text(
        "import wing.tools\n", encoding="utf-8"
    )
    cache = tmp_path / "__pycache__"
    cache.mkdir()
    (cache / "c.py").write_text("import wing\n", encoding="utf-8")
    venv = tmp_path / ".venv" / "lib"
    venv.mkdir(parents=True)
    (venv / "d.py").write_text("import wing\n", encoding="utf-8")

    violations = scan_tree(tmp_path)
    assert [Path(v.path).name for v in violations] == ["a.py", "b.py"]
    assert "forbidden import" in format_violations(violations)
