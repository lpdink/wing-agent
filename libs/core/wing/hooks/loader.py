# wing/hooks/loader.py
"""Load Python hook files whose decorators register handlers at import time."""

from pathlib import Path


def load_hooks(hooks_patterns: list[str]) -> int:
    """Load hook files from glob patterns.

    Each matched .py file is imported; hook registration happens
    via the ``hooks.on()`` decorator during import.
    """
    import glob
    import importlib
    import importlib.util

    from wing.common.logger import log

    failed = 0
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
                    failed += 1
                    log.warning(f"Cannot create import spec for hook file: {file_path}")
                    continue
                module = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(module)
                log.info(f"Loaded hook file: {file_path}")
            except Exception as e:
                failed += 1
                log.warning(f"Failed to load hook file {file_path}: {type(e).__name__}")
    return failed
