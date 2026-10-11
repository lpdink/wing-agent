"""日志设施的降级路径：轮转/清理/符号链接的失败都不得外溢。

日志是"最后一公里"：它自己在磁盘异常时必须静默降级（否则一次 ENOSPC 会让
整个进程在写日志时崩掉），且 asyncio 兜底处理器要把未处理异常转写进 wing 日志。
"""

from __future__ import annotations

import asyncio
import io
import logging
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Any, cast


from wing.common.logger import (
    NO_CORRELATION,
    _DailyFileHandler,
    _LoopExceptionLogger,
    _prune_old_logs,
    _update_new_symlink,
    install_loop_exception_logger,
    setup_logger,
)


def _record(level: int = logging.WARNING, msg: str = "hello") -> logging.LogRecord:
    return logging.LogRecord(
        name="wing",
        level=level,
        pathname=__file__,
        lineno=1,
        msg=msg,
        args=(),
        exc_info=None,
    )


class TestFormatter:
    def test_color_wrapped_for_known_levels(self):
        from wing.common.logger import _PathFormatter as _Formatter

        formatter = _Formatter(root=Path("/tmp"), use_color=True, context=None)
        assert formatter.format(_record(logging.WARNING)).endswith("\033[0m")
        assert "\033[93m" in formatter.format(_record(logging.WARNING))

    def test_no_color_keeps_plain_line(self):
        from wing.common.logger import _PathFormatter as _Formatter

        formatter = _Formatter(root=Path("/tmp"), use_color=False, context=None)
        assert "\033[" not in formatter.format(_record())


class TestSymlinkAndPrune:
    def test_symlink_failure_is_swallowed(self, tmp_path: Path, monkeypatch):
        def _denied(self: Path, target: str) -> None:
            raise OSError("symlink not supported")

        monkeypatch.setattr(Path, "symlink_to", _denied)

        _update_new_symlink(tmp_path, tmp_path / "wing_2026-01-01.log")  # 不抛

    def test_symlink_points_at_active_file(self, tmp_path: Path):
        target = tmp_path / "wing_2026-01-01.log"
        target.write_text("x", encoding="utf-8")

        _update_new_symlink(tmp_path, target)

        link = tmp_path / "new.log"
        assert link.is_symlink() and link.resolve() == target.resolve()

    def test_prune_deletes_only_expired(self, tmp_path: Path):
        today = date(2026, 1, 10)
        old = tmp_path / "wing_2026-01-01.log"
        fresh = tmp_path / "wing_2026-01-09.log"
        old.write_text("old", encoding="utf-8")
        fresh.write_text("fresh", encoding="utf-8")

        _prune_old_logs(tmp_path, today)

        assert not old.exists()
        assert fresh.exists()

    def test_prune_unlink_failure_is_swallowed(self, tmp_path: Path, monkeypatch):
        today = date(2026, 1, 10)
        (tmp_path / "wing_2026-01-01.log").write_text("old", encoding="utf-8")
        original_unlink = Path.unlink

        def _denied(self: Path, *args: Any, **kwargs: Any):
            raise OSError("busy")

        monkeypatch.setattr(Path, "unlink", _denied)
        try:
            _prune_old_logs(tmp_path, today)  # 不抛
        finally:
            monkeypatch.setattr(Path, "unlink", original_unlink)


class _ExplodingStream(io.StringIO):
    def close(self) -> None:
        raise OSError("close failed")


class TestDailyFileHandler:
    def test_rotate_creates_daily_file_and_symlink(self, tmp_path: Path):
        handler = _DailyFileHandler(tmp_path, now=lambda: datetime(2026, 1, 10, 9, 0))
        handler.emit(_record(msg="first"))

        assert (tmp_path / "wing_2026-01-10.log").exists()
        assert (tmp_path / "new.log").is_symlink()
        handler.close()

    def test_rotation_swallows_close_failure(self, tmp_path: Path):
        """轮转时关闭旧流失败：忽略并继续（旧 fd 由 OS 回收）。"""
        handler = _DailyFileHandler(tmp_path, now=lambda: datetime(2026, 1, 10, 9, 0))
        handler._stream = _ExplodingStream()
        handler._day = date(2026, 1, 9)  # 日期变了 → 触发轮转

        handler.emit(_record(msg="second"))

        # 旧流关闭失败被吞掉：新文件照常打开并写入
        assert "second" in (tmp_path / "wing_2026-01-10.log").read_text(
            encoding="utf-8"
        )
        handler.close()

    def test_close_swallows_stream_failure(self, tmp_path: Path):
        handler = _DailyFileHandler(tmp_path)
        handler._stream = _ExplodingStream()

        handler.close()  # 不抛

        assert handler._stream is None

    def test_rolls_to_new_day(self, tmp_path: Path):
        clock = {"now": datetime(2026, 1, 10, 23, 59)}
        handler = _DailyFileHandler(tmp_path, now=lambda: clock["now"])
        handler.emit(_record(msg="before"))
        clock["now"] = datetime(2026, 1, 11, 0, 1)
        handler.emit(_record(msg="after"))

        assert (tmp_path / "wing_2026-01-10.log").exists()
        assert (tmp_path / "wing_2026-01-11.log").exists()
        assert (tmp_path / "new.log").resolve() == (tmp_path / "wing_2026-01-11.log")
        handler.close()


class TestLoopExceptionLogger:
    def test_logs_then_forwards_to_previous(self, monkeypatch):
        calls: list[dict] = []

        def _previous(loop: Any, context: dict) -> None:
            calls.append(context)

        logger = _LoopExceptionLogger(_previous)
        logged: list[tuple] = []
        monkeypatch.setattr(
            "wing.common.logger.log",
            _StubLog(lambda msg, **kw: logged.append((msg, kw))),
        )

        logger(_stub_loop(), {"message": "Task exception was never retrieved"})

        assert logged and "Task exception was never retrieved" in logged[0][0]
        assert calls == [{"message": "Task exception was never retrieved"}]

    def test_log_failure_is_swallowed(self, monkeypatch):
        forwarded: list[dict] = []
        logger = _LoopExceptionLogger(lambda loop, ctx: forwarded.append(ctx))
        monkeypatch.setattr(
            "wing.common.logger.log",
            _StubLog(lambda msg, **kw: (_ for _ in ()).throw(RuntimeError("boom"))),
        )

        logger(_stub_loop(), {"message": "x"})  # 不抛

        assert forwarded == [{"message": "x"}]

    def test_falls_back_to_default_handler(self):
        contexts: list[dict] = []

        class _Loop:
            def default_exception_handler(self, context: dict) -> None:
                contexts.append(context)

        _LoopExceptionLogger(None)(_cast_loop(_Loop()), {"message": "orphan"})

        assert contexts == [{"message": "orphan"}]

    def test_install_is_idempotent(self):
        async def _scenario() -> None:
            install_loop_exception_logger()
            first = asyncio.get_running_loop().get_exception_handler()
            install_loop_exception_logger()
            assert asyncio.get_running_loop().get_exception_handler() is first
            asyncio.get_running_loop().set_exception_handler(None)

        asyncio.run(_scenario())

    def test_install_skips_outside_running_loop(self):
        install_loop_exception_logger()  # 无运行中的 loop：静默跳过


class _StubLoop:
    """loop 替身：只实现处理器转发所需的 default_exception_handler。"""

    def __init__(self) -> None:
        self.contexts: list[dict] = []

    def default_exception_handler(self, context: dict) -> None:
        self.contexts.append(context)


def _cast_loop(loop: object) -> "asyncio.AbstractEventLoop":
    return cast("asyncio.AbstractEventLoop", loop)


def _stub_loop() -> "asyncio.AbstractEventLoop":
    return _cast_loop(_StubLoop())


class _StubLog:
    def __init__(self, error) -> None:
        self._error = error

    def error(self, msg: str, **kwargs: Any) -> None:
        self._error(msg, **kwargs)


class TestSetupLogger:
    def test_file_handler_always_debug_and_correlation_opt_out(self, tmp_path: Path):
        logger = setup_logger(level="ERROR", log_dir=tmp_path, now=datetime.now)
        try:
            logger.warning("visible-in-file")
            logger.debug("debug-visible-in-file")
            logger.error(
                "loop fallback",
                extra={NO_CORRELATION: True},
            )
        finally:
            for handler in list(logger.handlers):
                handler.close()
            logger.handlers.clear()

        text = next(tmp_path.glob("wing_*.log")).read_text(encoding="utf-8")
        assert "visible-in-file" in text
        assert "debug-visible-in-file" in text
        assert "loop fallback" in text


def test_expired_dates_are_parsed_from_legacy_names(tmp_path: Path):
    """旧的按进程命名（wing_YYYY-MM-DD-HH-MM-SS.log）也能被清理识别。"""
    today = date(2026, 1, 10)
    legacy = tmp_path / "wing_2026-01-01-12-30-45.log"
    legacy.write_text("old", encoding="utf-8")

    _prune_old_logs(tmp_path, today - timedelta(days=0))

    assert not legacy.exists()
