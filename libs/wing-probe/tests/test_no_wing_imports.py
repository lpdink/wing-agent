"""门禁自证：``libs/wing-probe/`` 全源码不得 import wing。

覆盖 tasks 1.6 / spec「import 门禁」。每次 `test-probe` 运行都执行——
probe 与产品共享内部结构的退化必须以红灯暴露，而不是静默通过。
"""

from pathlib import Path

from wing_probe.guard import PKG_ROOT, format_violations, iter_python_files, scan_tree


def test_probe_sources_have_no_wing_imports() -> None:
    violations = scan_tree(PKG_ROOT)
    assert not violations, (
        "probe 源码禁止 import wing（含 wing.*）：\n" + format_violations(violations)
    )


def test_scan_surface_covers_package_and_tests() -> None:
    """扫描面自证：包源码与测试自身都在扫描范围内。"""
    scanned = {path.resolve() for path in iter_python_files(PKG_ROOT)}
    assert (PKG_ROOT / "wing_probe" / "guard.py").resolve() in scanned
    assert (PKG_ROOT / "tests" / "test_no_wing_imports.py").resolve() in scanned
    # 相对路径（tmp 下创建的样例）不参与断言，只证明扫描根正确。
    assert PKG_ROOT.is_dir()
    assert all(isinstance(path, Path) for path in scanned)
