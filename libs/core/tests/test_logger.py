"""Tests for the backend logging policy (wing.common.logger).

Policy under test (see docs/dev/config-logging.md):
- importing wing has no logging side effects (no files created);
- one append-mode file per local day, named wing_YYYY-MM-DD.log;
- new.log symlink always points at the active daily file;
- rotation at local midnight without restart;
- files older than 7 days are pruned.
"""

import asyncio
import logging
import os
import re
import subprocess
import sys
from collections.abc import Iterator
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest

from wing.common.logger import (
    RETENTION_DAYS,
    install_loop_exception_logger,
    setup_logger,
)


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


def test_malformed_date_names_never_crash(tmp_path: Path, _restore_logger) -> None:
    """Regex-shaped but calendar-invalid names must not raise.

    Regression: `wing_2026-13-45.log` once blew up date.fromisoformat inside
    _prune_old_logs, crashing gateway startup. Such files are foreign — they
    are skipped (left untouched), never deleted, and never fatal.
    """
    malformed = [
        "wing_2026-13-45.log",  # month 13, day 45
        "wing_2026-02-30.log",  # Feb 30
        "wing_2026-13-45-01-02-03.log",  # legacy per-process shape
    ]
    for name in malformed:
        (tmp_path / name).write_text("", encoding="utf-8")

    now = datetime(2026, 9, 8, 12, 0, 0)
    setup_logger(log_dir=tmp_path, now=lambda: now)  # must not raise
    _log("still alive")

    assert "still alive" in (tmp_path / "wing_2026-09-08.log").read_text(
        encoding="utf-8"
    )
    for name in malformed:
        assert (tmp_path / name).exists(), f"{name} should be left untouched"


def test_relpath_is_cached_per_source_file(tmp_path: Path, _restore_logger) -> None:
    """relpath 记忆化：同一源文件的多条日志各自带自己的行号。

    回归：缓存若存的是「路径:行号」整串，同一文件的第二条日志会复用第一条的
    行号（错误归因）；缓存必须只存相对路径，行号逐条拼接。同时覆盖 root 之外
    的文件（回退绝对路径，不抛 ValueError）。
    """
    from wing.common.logger import _PathFormatter

    # root 与生产同口径（logger.py 里也是 resolve 过的），否则 repo 路径含
    # symlink 时 relative_to 会回退绝对路径，第一条断言退化成子串巧合。
    formatter = _PathFormatter(Path(__file__).resolve().parent, use_color=False)

    def record(pathname: str, lineno: int) -> logging.LogRecord:
        return logging.LogRecord(
            name="wing",
            level=logging.INFO,
            pathname=pathname,
            lineno=lineno,
            msg="m",
            args=(),
            exc_info=None,
        )

    own_file = str(Path(__file__).resolve())
    first = formatter.format(record(own_file, 11))
    second = formatter.format(record(own_file, 22))
    assert "test_logger.py:11" in first
    assert "test_logger.py:22" in second, (
        "同一文件的第二条日志复用了第一条的行号——缓存粒度错了"
    )

    # root 之外的文件回退绝对路径。用 tmp 目录的兄弟文件而不是 /etc：后者在
    # macOS 上经 realpath 会变成 /private/etc，子串断言即使实现写成相对路径
    # （`os.path.relpath`）也会通过——弱 oracle。
    outside_path = (tmp_path.parent / "wing-outside.py").resolve()
    outside = formatter.format(record(str(outside_path), 7))
    # 用「 - <路径> - 」的字段边界断言，而不是子串：子串断言会被
    # `/etc → /private/etc` 这类 realpath 语义或相对路径实现蒙混过关。
    assert f" - {outside_path}:7 - " in outside


@pytest.fixture()
def wing_logs(caplog: pytest.LogCaptureFixture) -> Iterator[pytest.LogCaptureFixture]:
    """捕获 wing logger 的记录（库 logger propagate=False，需挂 handler）。"""
    caplog.set_level(logging.DEBUG, logger="wing")
    logger = logging.getLogger("wing")
    logger.addHandler(caplog.handler)
    yield caplog
    logger.removeHandler(caplog.handler)


class TestLoopExceptionLogger:
    """asyncio 未处理异常的兜底日志（转发给原处理器）。"""

    @pytest.mark.asyncio
    async def test_logs_and_chains_to_previous_handler(self, wing_logs) -> None:
        loop = asyncio.get_running_loop()
        chained: list[dict[str, Any]] = []
        original = loop.get_exception_handler()
        loop.set_exception_handler(lambda _loop, context: chained.append(context))
        try:
            install_loop_exception_logger()
            handler = loop.get_exception_handler()
            assert handler is not None
            error = RuntimeError("probe boom")
            handler(
                loop,
                {
                    "message": "Task exception was never retrieved",
                    "exception": error,
                    "task": asyncio.current_task(),
                },
            )
            # 幂等：重复安装不叠处理器（依旧是同一个 handler）
            install_loop_exception_logger()
            assert loop.get_exception_handler() is handler
            handler(loop, {"message": "second"})
        finally:
            loop.set_exception_handler(original)

        messages = [record.message for record in wing_logs.records]
        assert sum("asyncio unhandled" in message for message in messages) == 2
        assert any("Task exception was never retrieved" in m for m in messages)
        assert "probe boom" in wing_logs.text
        # 原处理器（stderr 行为）被原样链上。
        assert [context["message"] for context in chained] == [
            "Task exception was never retrieved",
            "second",
        ]

    def test_no_running_loop_is_a_noop(self) -> None:
        """无运行中的事件循环时静默跳过（库导入零副作用）。"""
        install_loop_exception_logger()  # 不抛即通过
