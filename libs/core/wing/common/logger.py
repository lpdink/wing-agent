"""
Logging module for OpenWing.
"""

import logging
import sys
from datetime import datetime
from pathlib import Path

# ANSI color codes
_COLORS = {
    "DEBUG": "\033[90m",
    "INFO": "\033[97m",
    "WARNING": "\033[93m",
    "ERROR": "\033[91m",
    "CRITICAL": "\033[91;1m",
}

_MAX_LOG_FILES = 7


class _PathFormatter(logging.Formatter):
    """Formatter with relative path support."""

    def __init__(self, root: Path, use_color: bool = False):
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


def _rotate_logs(log_dir: Path, current_log: Path) -> None:
    """Maintain log files: keep newest 7, update new.log symlink."""
    # Update symlink
    symlink = log_dir / "new.log"
    if symlink.exists() or symlink.is_symlink():
        symlink.unlink()
    try:
        symlink.symlink_to(current_log.name)
    except OSError:
        pass  # Symlink creation failed, not critical

    # Clean old logs
    logs = sorted(
        log_dir.glob("wing_*.log"), key=lambda p: p.stat().st_mtime, reverse=True
    )
    for old_log in logs[_MAX_LOG_FILES:]:
        try:
            old_log.unlink()
        except OSError:
            pass


def setup_logger(level: str = "WARNING", path: str | Path | None = None) -> logging.Logger:
    """Setup and return configured logger.

    Args:
        level: Console log level, default WARNING.
        path: Log directory, default ``$WING_HOME/core/logs``.
    """
    logger = logging.getLogger("wing")
    logger.handlers.clear()
    logger.setLevel(logging.DEBUG)  # Logger captures all, handlers filter

    root = Path(__file__).parent.resolve()

    # Console handler - controlled by config
    console = logging.StreamHandler(sys.stdout)
    console.setLevel(getattr(logging, level.upper(), logging.INFO))
    console.setFormatter(_PathFormatter(root, use_color=True))
    logger.addHandler(console)

    # File handler - always DEBUG level
    from wing.config import get_wing_home

    log_dir = Path(path).expanduser() if path else get_wing_home() / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)

    timestamp = datetime.now().strftime("%Y-%m-%d-%H-%M-%S")
    log_file = log_dir / f"wing_{timestamp}.log"

    file_handler = logging.FileHandler(log_file, encoding="utf-8")
    file_handler.setLevel(logging.DEBUG)
    file_handler.setFormatter(_PathFormatter(root))
    logger.addHandler(file_handler)

    # Force immediate write
    def _force_flush() -> None:
        if file_handler.stream is not None:
            file_handler.stream.flush()

    file_handler.flush = _force_flush  # ty: ignore

    _rotate_logs(log_dir, log_file)

    return logger


log = setup_logger()
