"""Tests for the backend logging policy (wing.common.logger).

Policy under test (see AGENTS.md "Logging"):
- importing wing has no logging side effects (no files created);
- one append-mode file per local day, named wing_YYYY-MM-DD.log;
- new.log symlink always points at the active daily file;
- rotation at local midnight without restart;
- files older than 7 days are pruned.
"""

import logging
import os
import re
import subprocess
import sys
from datetime import date, datetime, timedelta
from pathlib import Path

import pytest

from wing.common.logger import RETENTION_DAYS, setup_logger


@pytest.fixture()
def _restore_logger():
    """Snapshot/restore the global wing logger around each test."""
    logger = logging.getLogger("wing")
    handlers = list(logger.handlers)
    level = logger.level
    propagate = logger.propagate
    yield logger
    for handler in logger.handlers[:]:
        logger.removeHandler(handler)
        handler.close()
    for handler in handlers:
        logger.addHandler(handler)
    logger.setLevel(level)
    logger.propagate = propagate


def _log(message: str) -> None:
    logging.getLogger("wing").info(message)


def test_import_has_no_side_effects(tmp_path: Path) -> None:
    """Importing wing must never create log files or attach file handlers."""
    code = (
        "import logging, pathlib, sys;"
        "import wing.common.logger;"
        "logger = logging.getLogger('wing');"
        "assert all(isinstance(h, logging.NullHandler) for h in logger.handlers), logger.handlers;"
        "assert not logger.propagate;"
        "logger.info('dropped before setup');"
        "logs = pathlib.Path(sys.argv[1]) / 'core' / 'logs';"
        "assert not logs.exists(), f'import created {logs}'"
    )
    env = {**os.environ, "WING_HOME": str(tmp_path)}
    subprocess.run(
        [sys.executable, "-c", code, str(tmp_path)],
        check=True,
        env=env,
        capture_output=True,
        timeout=30,
    )


def test_daily_file_naming_and_new_symlink(
    tmp_path: Path, _restore_logger: logging.Logger
) -> None:
    now = datetime(2026, 9, 8, 23, 0, 0)
    setup_logger(log_dir=tmp_path, now=lambda: now)

    _log("hello wing")

    daily = tmp_path / "wing_2026-09-08.log"
    assert daily.exists()
    content = daily.read_text(encoding="utf-8")
    assert "hello wing" in content
    # Grep-friendly prefix: every line starts with the local timestamp.
    assert re.match(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} - INFO - ", content)

    new_log = tmp_path / "new.log"
    assert new_log.is_symlink()
    assert new_log.resolve() == daily.resolve()


def test_rotation_at_local_midnight(tmp_path: Path, _restore_logger) -> None:
    clock = {"now": datetime(2026, 9, 8, 23, 59, 59)}
    setup_logger(log_dir=tmp_path, now=lambda: clock["now"])

    _log("day one")
    clock["now"] = datetime(2026, 9, 9, 0, 0, 1)
    _log("day two")

    day1 = tmp_path / "wing_2026-09-08.log"
    day2 = tmp_path / "wing_2026-09-09.log"
    assert day1.read_text(encoding="utf-8").endswith("day one\n")
    assert "day two" in day2.read_text(encoding="utf-8")
    # Symlink follows the rotation.
    assert (tmp_path / "new.log").resolve() == day2.resolve()


def test_restart_appends_to_same_day_file(tmp_path: Path, _restore_logger) -> None:
    now = datetime(2026, 9, 8, 10, 0, 0)
    setup_logger(log_dir=tmp_path, now=lambda: now)
    _log("before restart")

    # Simulate a gateway restart on the same day.
    setup_logger(log_dir=tmp_path, now=lambda: now)
    _log("after restart")

    content = (tmp_path / "wing_2026-09-08.log").read_text(encoding="utf-8")
    assert "before restart" in content
    assert "after restart" in content


def test_prune_older_than_retention(tmp_path: Path, _restore_logger) -> None:
    today = date(2026, 9, 8)
    cutoff = today - timedelta(days=RETENTION_DAYS - 1)

    expired = [
        tmp_path / "wing_2026-08-30.log",  # current naming, too old
        tmp_path / "wing_2026-08-31-01-02-03.log",  # legacy per-process naming
    ]
    kept = [
        tmp_path / f"wing_{cutoff:%Y-%m-%d}.log",  # exactly on the cutoff
        tmp_path / "wing_2026-09-08.log",
    ]
    unrelated = tmp_path / "wing_not-a-date.log"  # unparseable → untouched
    for path in [*expired, *kept, unrelated]:
        path.write_text("", encoding="utf-8")

    now = datetime(2026, 9, 8, 12, 0, 0)
    setup_logger(log_dir=tmp_path, now=lambda: now)
    _log("trigger rotation")

    for path in expired:
        assert not path.exists(), f"{path.name} should have been pruned"
    for path in kept:
        assert path.exists(), f"{path.name} should have been kept"
    assert unrelated.exists()
