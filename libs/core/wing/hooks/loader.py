# wing/hooks/loader.py
"""hook 文件加载器（11 归位：自 ``wing/config/loader.py`` 抽出）。

每个匹配的 ``.py`` 文件被 import；hook 注册在 import 期由 ``hooks.on()``
装饰器完成——加载器只执行文件，不注册任何 handler。

函数体逐字搬迁（含函数内的 glob / importlib / log 懒加载）；新模块补一个
模块级 ``Path`` import（原函数依赖 ``config/loader.py`` 的模块级 ``Path``）。
"""

from pathlib import Path


def load_hooks(hooks_patterns: list[str]) -> None:
    """Load hook files from glob patterns.

    Each matched .py file is imported; hook registration happens
    via the ``hooks.on()`` decorator during import.
    """
    import glob
    import importlib
    import importlib.util

    from wing.common.logger import log

    for pattern in hooks_patterns:
        expanded = Path(pattern).expanduser()
        matched_files = sorted(glob.glob(str(expanded)))
        if not matched_files:
            log.info(f"No hook files matched pattern: {pattern}")
            continue

        for file_path in matched_files:
            file_path = Path(file_path)
            if not file_path.is_file() or file_path.suffix != ".py":
                continue

            module_name = f"wing_hook_{file_path.stem}"

            try:
                spec = importlib.util.spec_from_file_location(
                    module_name, str(file_path)
                )
                if spec is None or spec.loader is None:
                    log.warning(f"Cannot create import spec for hook file: {file_path}")
                    continue
                module = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(module)
                log.info(f"Loaded hook file: {file_path}")
            except Exception as e:
                log.warning(f"Failed to load hook file {file_path}: {e}")
