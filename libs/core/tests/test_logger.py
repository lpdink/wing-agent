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
from typing import Any, cast

import pytest

from wing.common.logger import (
    RETENTION_DAYS,
    LogContextProvider,
    install_loop_exception_logger,
    setup_logger,
)
from wing.request_context import (
    get_request_context,
    reset_request_context,
    set_request_context,
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
    # Path field is root-relative, not the absolute source path.
    assert re.search(r" - INFO - .*tests/test_logger\.py:\d+ - hello wing", content)

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


def _production_formatter(logger: logging.Logger):
    """取 setup_logger 挂上的 formatter（= 生产 root 计算逻辑本体）。"""
    from wing.common.logger import _PathFormatter

    return next(
        handler.formatter
        for handler in logger.handlers
        if isinstance(handler.formatter, _PathFormatter)
    )


def test_package_files_render_relative_to_wing_package(
    tmp_path: Path, _restore_logger: logging.Logger
) -> None:
    """包内源码渲染为 ``wing/...`` 相对形态（回归：root 曾算成 wing/common）。

    除 ``wing/common/`` 自身外，旧 root 让所有文件 `relative_to` 失败、整片
    回退绝对路径（安装态下就是 site-packages 长路径）。这里直接用生产
    formatter 格式化一条「源码位于 wing/provider/openai/provider.py」的记录
    ——root 的正确性必须由生产表达式负责，而不是测试里复刻的近似式。
    """
    setup_logger(log_dir=tmp_path)
    formatter = _production_formatter(_restore_logger)

    provider_file = (
        Path(__file__).resolve().parents[1]
        / "wing"
        / "provider"
        / "openai"
        / "provider.py"
    )
    record = logging.LogRecord(
        name="wing",
        level=logging.INFO,
        pathname=str(provider_file),
        lineno=328,
        msg="[DONE] openai_compat response header received",
        args=(),
        exc_info=None,
    )
    line = formatter.format(record)

    assert " - wing/provider/openai/provider.py:328 - " in line
    assert str(provider_file) not in line


def test_correlation_section_rendered_from_context(
    tmp_path: Path, _restore_logger: logging.Logger
) -> None:
    """session / request id 从协程上下文逐条读取并标注在行内。"""
    setup_logger(log_dir=tmp_path, context=get_request_context)

    token = set_request_context(
        session_id="20261009-223452-932fd6d1", request_id="wing_12"
    )
    try:
        _log("hello correlation")
    finally:
        reset_request_context(token)

    content = (tmp_path / "new.log").read_text(encoding="utf-8")
    assert re.search(
        r" - INFO - \[20261009-223452-932fd6d1 wing_12\] - "
        r"tests/test_logger\.py:\d+ - hello correlation$",
        content,
        re.MULTILINE,
    ), content


def test_correlation_renders_only_available_ids(
    tmp_path: Path, _restore_logger: logging.Logger
) -> None:
    """只有一个 id 时只标注一个；都没有时整段省略（不留空壳）。"""
    setup_logger(log_dir=tmp_path, context=get_request_context)

    for context, message in [
        ({"session_id": "sid-only"}, "session only"),
        ({"request_id": "rid-only"}, "request only"),
    ]:
        token = set_request_context(**context)
        try:
            _log(message)
        finally:
            reset_request_context(token)
    _log("no context")

    content = (tmp_path / "new.log").read_text(encoding="utf-8")
    assert " - INFO - [sid-only] - tests/test_logger.py:" in content
    assert " - INFO - [rid-only] - tests/test_logger.py:" in content
    # 无上下文的行保持既有形状：级别后直接是路径字段。
    assert re.search(
        r" - INFO - tests/test_logger\.py:\d+ - no context$", content, re.MULTILINE
    )
    assert "[]" not in content


def test_context_provider_failure_degrades_to_no_section(
    tmp_path: Path, _restore_logger: logging.Logger
) -> None:
    """provider 抛错按「无上下文」处理——日志格式化不得外溢异常。"""

    def boom():
        raise RuntimeError("provider down")

    setup_logger(log_dir=tmp_path, context=boom)
    _log("survived")

    content = (tmp_path / "new.log").read_text(encoding="utf-8")
    assert re.search(
        r" - INFO - tests/test_logger\.py:\d+ - survived$", content, re.MULTILINE
    ), content


def test_context_provider_attribute_failure_keeps_line(
    tmp_path: Path, _restore_logger: logging.Logger
) -> None:
    """provider 返回对象的属性访问抛错：按「无上下文」处理，整行照常落盘。

    回归：护栏曾只包 ``provider()`` 调用本身，属性读取外溢后 logging 的
    ``handleError`` 会把该行从文件与 stdout 一起丢掉——丢日志比丢标注严重。
    """

    class BadContext:
        @property
        def session_id(self) -> str:
            raise RuntimeError("attribute boom")

        request_id = None

    # BadContext 刻意违反 LogContext 契约（属性访问抛错），cast 只为绕过静态
    # 检查——本测试要验证的正是"契约被违反时也不能出事"。
    setup_logger(
        log_dir=tmp_path, context=cast(LogContextProvider, lambda: BadContext())
    )
    _log("survived attr")

    content = (tmp_path / "new.log").read_text(encoding="utf-8")
    assert re.search(
        r" - INFO - tests/test_logger\.py:\d+ - survived attr$", content, re.MULTILINE
    ), content


def test_loop_exception_logs_never_correlate(
    tmp_path: Path, _restore_logger: logging.Logger
) -> None:
    """事件循环兜底日志不参与关联标注——GC 时机的上下文可能属于无关任务。

    回归：带上下文的协程触发兜底处理器时，该行曾被错误打上"恰好路过"任务的
    session / request（错误归属比不归属更误导）。
    """
    from wing.common.logger import _LoopExceptionLogger

    setup_logger(log_dir=tmp_path, context=get_request_context)

    loop = asyncio.new_event_loop()
    try:
        token = set_request_context(
            session_id="unrelated-sid", request_id="unrelated-rid"
        )
        try:
            handler = _LoopExceptionLogger(lambda _loop, _context: None)
            handler(loop, {"message": "Task exception was never retrieved"})
        finally:
            reset_request_context(token)
    finally:
        loop.close()

    content = (tmp_path / "new.log").read_text(encoding="utf-8")
    assert "asyncio unhandled" in content
    assert "unrelated-sid" not in content
    assert "unrelated-rid" not in content
    assert re.search(
        r" - ERROR - wing/common/logger\.py:\d+ - asyncio unhandled",
        content,
        re.MULTILINE,
    ), content


def test_setup_logger_survives_missing_stdout(
    tmp_path: Path,
    _restore_logger: logging.Logger,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``sys.stdout is None``（pythonw / 嵌入式宿主）不得变成启动崩溃路径。"""
    monkeypatch.setattr(sys, "stdout", None)

    setup_logger(log_dir=tmp_path)  # 不抛即通过

    console = logging.getLogger("wing").handlers[0]
    assert isinstance(console, logging.StreamHandler)


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
