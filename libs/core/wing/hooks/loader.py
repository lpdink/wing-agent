# wing/hooks/loader.py
"""Load Python hook files whose decorators register handlers at import time."""

from dis import get_instructions
from pathlib import Path


def _hook_failure(path: Path, exc: Exception) -> str:
    source = str(path.absolute())
    if isinstance(exc, SyntaxError) and exc.filename == source and exc.lineno:
        return f"{str(path)!r}:{exc.lineno}: invalid Python syntax"

    line = None
    module = None
    trace = exc.__traceback__
    while trace is not None:
        code = trace.tb_frame.f_code
        if code.co_filename == source:
            line = trace.tb_lineno
            if isinstance(exc, ModuleNotFoundError):
                instruction = next(
                    (
                        item
                        for item in get_instructions(code)
                        if item.offset == trace.tb_lasti
                    ),
                    None,
                )
                # The import operand is a source literal. exc.name can come from config.
                if instruction is not None and instruction.opname == "IMPORT_NAME":
                    module = instruction.argval
        trace = trace.tb_next

    location = f"{str(path)!r}:{line}" if line is not None else repr(str(path))
    if isinstance(exc, ModuleNotFoundError):
        return (
            f"{location}: missing Python module for import {module!r}"
            if module
            else f"{location}: missing Python module"
        )
    return f"{location}: {type(exc).__name__}"


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
                log.warning(f"Failed to load hook file {_hook_failure(file_path, e)}")
    return failed
