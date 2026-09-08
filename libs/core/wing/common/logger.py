"""
Logging for the wing backend.

Policy (kept in sync with the TUI frontend, see AGENTS.md "Logging"):

- One file per **local** calendar day: ``wing_YYYY-MM-DD.log`` under
  ``$WING_HOME/core/logs/`` (default ``~/.wing/core/logs/``), opened in
  append mode so gateway restarts never truncate or fork the log.
- The ``new.log`` symlink in that directory always points at the active
  daily file (refreshed on every rotation and on every setup).
- Files older than ``RETENTION_DAYS`` days are pruned on setup and rotation.
- **No import side effects**: importing ``wing`` never touches the
  filesystem — file/console handlers attach only via :func:`setup_logger`,
  called explicitly by the gateway CLI entry point.
"""

from __future__ import annotations

import logging
import re
import sys
from collections.abc import Callable
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import IO

_LOGGER_NAME = "wing"
RETENTION_DAYS = 7

# Current `wing_YYYY-MM-DD.log` and legacy per-process `wing_YYYY-MM-DD-HH-MM-SS.log`.
_LOG_NAME_RE = re.compile(r"^wing_(\d{4}-\d{2}-\d{2})(?:-\d{2}-\d{2}-\d{2})?\.log$")

# ANSI color codes
_COLORS = {
    "DEBUG": "\033[90m",
    "INFO": "\033[97m",
    "WARNING": "\033[93m",
    "ERROR": "\033[91m",
    "CRITICAL": "\033[91;1m",
}


class _PathFormatter(logging.Formatter):
    """Formatter with relative path support."""

    def __init__(self, root: Path, use_color: bool = False) -> None:
        super().__init__(
            "%(asctime)s - %(levelname)s - %(relpath)s - %(message)s",
            "%Y-%m-%d %H:%M:%S",
        )
        self._root = root
        self._use_color = use_color

    def format(self, record: logging.LogRecord) -> str:
        abs_path = Path(record.pathname).resolve()
        try:
            rel = abs_path.relative_to(self._root)
        except ValueError:
            rel = abs_path
        record.relpath = f"{rel}:{record.lineno}"

        msg = super().format(record)
        if self._use_color and record.levelname in _COLORS:
            return f"{_COLORS[record.levelname]}{msg}\033[0m"
        return msg


def _daily_file(log_dir: Path, day: date) -> Path:
    return log_dir / f"wing_{day:%Y-%m-%d}.log"


def _update_new_symlink(log_dir: Path, target: Path) -> None:
    """Point ``new.log`` at the active daily file (best-effort)."""
    symlink = log_dir / "new.log"
    try:
        if symlink.is_symlink() or symlink.exists():
            symlink.unlink()
        symlink.symlink_to(target.name)
    except OSError:
        pass  # Symlink maintenance failed, not critical


def _parse_log_date(name: str) -> date | None:
    """Extract the day encoded in a log file name, if any.

    Accepts the current daily naming and the legacy per-process naming.
    Malformed dates (e.g. ``wing_2026-13-45.log`` — regex-shaped but not a
    real calendar day) yield ``None`` so pruning never raises on foreign
    files, mirroring the Rust side's ``parse_log_date``.
    """
    match = _LOG_NAME_RE.match(name)
    if match is None:
        return None
    try:
        return date.fromisoformat(match.group(1))
    except ValueError:
        return None


def _prune_old_logs(log_dir: Path, today: date) -> None:
    """Delete daily log files older than the retention window."""
    cutoff = today - timedelta(days=RETENTION_DAYS - 1)
    for path in log_dir.glob("wing_*.log"):
        day = _parse_log_date(path.name)
        if day is not None and day < cutoff:
            try:
                path.unlink()
            except OSError:
                pass


class _DailyFileHandler(logging.Handler):
    """Append-only daily log file named by local date.

    The file is opened lazily on the first record (silent processes never
    create empty files) and re-checked on every emit, so a long-running
    gateway rolls at local midnight without a restart.
    """

    def __init__(
        self, log_dir: Path, now: Callable[[], datetime] = datetime.now
    ) -> None:
        super().__init__()
        self._log_dir = log_dir
        self._now = now
        self._day: date | None = None
        self._stream: IO[str] | None = None

    def emit(self, record: logging.LogRecord) -> None:
        try:
            day = self._now().date()
            if day != self._day:
                self._rotate(day)
            stream = self._stream
            if stream is not None:
                stream.write(self.format(record) + "\n")
                stream.flush()
        except Exception:  # noqa: BLE001 - logging must never raise
            self.handleError(record)

    def _rotate(self, day: date) -> None:
        if self._stream is not None:
            try:
                self._stream.close()
            except OSError:
                pass
            self._stream = None
        self._log_dir.mkdir(parents=True, exist_ok=True)
        target = _daily_file(self._log_dir, day)
        self._stream = target.open("a", encoding="utf-8")
        self._day = day
        _update_new_symlink(self._log_dir, target)
        _prune_old_logs(self._log_dir, day)

    def close(self) -> None:
        if self._stream is not None:
            try:
                self._stream.close()
            except OSError:
                pass
            self._stream = None
        super().close()


def setup_logger(
    level: str = "WARNING",
    log_dir: str | Path | None = None,
    *,
    now: Callable[[], datetime] = datetime.now,
) -> logging.Logger:
    """Attach console + daily-file handlers to the ``wing`` logger.

    Called explicitly by the gateway CLI; importing ``wing`` alone never
    writes log files.

    Args:
        level: Console log level (stdout); the daily file always logs DEBUG.
        log_dir: Log directory, default ``$WING_HOME/core/logs``.
        now: Clock override for tests.
    """
    logger = logging.getLogger(_LOGGER_NAME)
    logger.handlers.clear()
    logger.setLevel(logging.DEBUG)  # Logger captures all, handlers filter
    logger.propagate = False

    root = Path(__file__).parent.resolve()

    # Console handler - controlled by config
    console = logging.StreamHandler(sys.stdout)
    console.setLevel(getattr(logging, level.upper(), logging.INFO))
    console.setFormatter(_PathFormatter(root, use_color=True))
    logger.addHandler(console)

    # File handler - always DEBUG level, one file per local day
    if log_dir is None:
        from wing.config import get_wing_home

        log_dir = get_wing_home() / "logs"
    log_dir = Path(log_dir).expanduser()

    file_handler = _DailyFileHandler(log_dir, now=now)
    file_handler.setLevel(logging.DEBUG)
    file_handler.setFormatter(_PathFormatter(root))
    logger.addHandler(file_handler)

    # Prune stale files at startup, even before the first record.
    log_dir.mkdir(parents=True, exist_ok=True)
    _prune_old_logs(log_dir, now().date())

    return logger


# Library logger — inert (NullHandler, no propagation) until setup_logger()
# is called by the gateway CLI. Importing wing never touches the filesystem.
log = logging.getLogger(_LOGGER_NAME)
log.addHandler(logging.NullHandler())
log.propagate = False
