"""Bash 工具本体：危险命令确认链、执行/超时/打断语义。

`is_dangerous_command` 的判定逻辑已有测试（test_dangerous_command.py），这里测
命令执行侧的路由与拆卸：确认对话框的四种归宿、超时后进程组清理、取消时
不留迟到副作用、中断钩子的注册/注销配对。
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import cast

import pytest

from wing.schema import ToolError
from wing.tools.builtin.bash import (
    FEEDBACK_TIMEOUT,
    _drain_pipes,
    _format_result,
    _format_timeout_result,
    _parse_feedback,
    execute_shell,
)

pytestmark = pytest.mark.asyncio


class _StubCtx:
    """最小 ToolContext 替身：记录 ask_feedback 调用与中断钩子生命周期。"""

    def __init__(
        self,
        *,
        yolo: bool = False,
        cwd: Path | None = None,
        answers: list[str] | Exception | None = None,
    ) -> None:
        self.session_id = "sess-bash-test"
        self.yolo = yolo
        self.cwd = cwd
        self._answers: list = (
            []
            if answers is None
            else (answers if isinstance(answers, list) else [answers])
        )
        self.asks: list[tuple] = []
        self.events: list = []
        self.hooks: dict[str, object] = {}
        self._hook_seq = 0

    # ── ToolContext 面 ──
    def set_yolo(self, value: bool) -> None:
        self.yolo = value

    def set_cwd(self, path: Path | None) -> None:
        self.cwd = path

    async def ask_feedback(self, event, timeout: float) -> str:
        self.asks.append((event, timeout))
        answer = self._answers.pop(0) if self._answers else "y"
        if isinstance(answer, Exception):
            raise answer
        return answer

    def emit(self, event) -> None:
        self.events.append(event)

    def register_interrupt_hook(self, hook) -> str:
        self._hook_seq += 1
        hook_id = f"hook-{self._hook_seq}"
        self.hooks[hook_id] = hook
        return hook_id

    def unregister_interrupt_hook(self, hook_id: str) -> None:
        self.hooks.pop(hook_id, None)


async def _pgrep(pattern: str) -> list[str]:
    proc = await asyncio.create_subprocess_shell(
        f"pgrep -f '{pattern}' || true",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    out, _ = await proc.communicate()
    return [line for line in out.decode().splitlines() if line.strip()]


# ── 路由：危险命令 / 白名单 ────────────────────────────────────


class TestExecuteShellRouting:
    async def test_yolo_skips_confirmation(self, tmp_path):
        ctx = _StubCtx(yolo=True, cwd=tmp_path)

        result = await execute_shell("echo skip-ask", ctx=ctx)

        assert "skip-ask" in result and ctx.asks == []

    async def test_whitelisted_command_skips_confirmation(self, tmp_path, monkeypatch):
        monkeypatch.setattr(
            "wing.tools.builtin.bash.is_dangerous_command", lambda cmd: False
        )
        ctx = _StubCtx(yolo=False, cwd=tmp_path)

        result = await execute_shell("echo safe", ctx=ctx)

        assert "safe" in result and ctx.asks == []

    async def test_dangerous_command_asks_before_running(self, tmp_path):
        ctx = _StubCtx(yolo=False, cwd=tmp_path, answers=["y"])

        result = await execute_shell("echo confirmed", ctx=ctx)

        assert "confirmed" in result
        ((event, timeout),) = ctx.asks
        assert timeout == FEEDBACK_TIMEOUT
        assert event.required is True
        assert "Dangerous command detected" in event.question
        assert "echo confirmed" in event.question


class TestDangerousCommandAnswers:
    async def test_no_rejects_command(self, tmp_path):
        ctx = _StubCtx(cwd=tmp_path, answers=["n"])

        with pytest.raises(ToolError, match="Command rejected by user"):
            await execute_shell("echo nope", ctx=ctx)

    async def test_yolo_answer_enables_yolo_and_runs(self, tmp_path):
        ctx = _StubCtx(cwd=tmp_path, answers=["yolo"])

        result = await execute_shell("echo yolo-run", ctx=ctx)

        assert "yolo-run" in result
        assert ctx.yolo is True  # 记忆到会话，后续命令不再询问

    async def test_invalid_answer_reprompts(self, tmp_path):
        ctx = _StubCtx(cwd=tmp_path, answers=["maybe", "Y"])

        result = await execute_shell("echo retried", ctx=ctx)

        assert "retried" in result
        assert len(ctx.asks) == 2
        assert ctx.asks[0][0].question.startswith("⚠️ Dangerous command detected")
        assert ctx.asks[1][0].question == "⚠️ Please choose one of the options."

    async def test_feedback_timeout_fails_closed(self, tmp_path):
        ctx = _StubCtx(cwd=tmp_path, answers=asyncio.TimeoutError())

        with pytest.raises(ToolError, match="Feedback timeout"):
            await execute_shell("echo never", ctx=ctx)

    @pytest.mark.parametrize(
        ("raw", "expected"),
        [
            ("y", "y"),
            (" Y ", "y"),
            ("N", "n"),
            ("YOLO", "yolo"),
            ("yes", None),
            ("", None),
        ],
    )
    async def test_parse_feedback(self, raw, expected):
        assert _parse_feedback(raw) == expected


# ── 执行：成功 / 失败 / 超时 / 异常 ────────────────────────────


class TestExecuteCommand:
    async def test_success_includes_exit_code_and_elapsed(self, tmp_path):
        ctx = _StubCtx(yolo=True, cwd=tmp_path)

        result = await execute_shell("echo out; echo err >&2", ctx=ctx)

        assert "out" in result and "[stderr] err" in result
        assert result.startswith("[exit code: 0 | ")
        assert ctx.hooks == {}  # 中断钩子注销

    async def test_nonzero_exit_raises_with_output(self, tmp_path):
        ctx = _StubCtx(yolo=True, cwd=tmp_path)

        with pytest.raises(ToolError) as excinfo:
            await execute_shell("echo partial; exit 3", ctx=ctx)

        assert "[exit code: 3 | " in str(excinfo.value)
        assert "partial" in str(excinfo.value)
        assert ctx.hooks == {}

    async def test_env_carries_session_id(self, tmp_path):
        ctx = _StubCtx(yolo=True, cwd=tmp_path)

        result = await execute_shell("echo $WING_SESSION_ID", ctx=ctx)

        assert "sess-bash-test" in result

    async def test_timeout_kills_process_group_and_reports_partial(self, tmp_path):
        ctx = _StubCtx(yolo=True, cwd=tmp_path)

        with pytest.raises(ToolError) as excinfo:
            await execute_shell("echo before; sleep 3737", ctx=ctx, timeout=1)

        message = str(excinfo.value)
        assert message.startswith("[exit code: -1]")
        assert "Command timed out after" in message
        assert "--- partial stdout ---" in message and "before" in message
        assert await _pgrep("slee[p] 3737") == []

    async def test_spawn_failure_wrapped_as_tool_error(self, tmp_path):
        ctx = _StubCtx(yolo=True, cwd=tmp_path / "gone")

        with pytest.raises(ToolError, match="Error executing command"):
            await execute_shell("echo x", ctx=ctx)

    async def test_cancellation_kills_process_and_propagates(self, tmp_path):
        ctx = _StubCtx(yolo=True, cwd=tmp_path)

        task = asyncio.create_task(execute_shell("sleep 3737", ctx=ctx, timeout=60))
        await asyncio.sleep(0.3)  # 让子进程起来
        task.cancel()

        with pytest.raises(asyncio.CancelledError):
            await task

        assert await _pgrep("slee[p] 3737") == []
        assert ctx.hooks == {}


# ── 结果格式化 / 管道排空 ─────────────────────────────────────


class TestFormatting:
    async def test_result_without_output_is_exit_code_only(self):
        assert _format_result(0, b"", b"", 2) == "[exit code: 0 | 2s]"

    async def test_result_with_stderr_only(self):
        assert _format_result(1, b"", b"bad", 1) == (
            "[exit code: 1 | 1s]\n[stderr] bad"
        )

    async def test_timeout_result_without_partial_output(self):
        assert _format_timeout_result(b"", b"", 5) == (
            "[exit code: -1]\nError: Command timed out after 5s"
        )

    async def test_timeout_result_includes_both_streams(self):
        out = _format_timeout_result(b"out", b"err", 5)

        assert "--- partial stdout ---\nout" in out
        assert "--- partial stderr ---\nerr" in out


class _FakeStream:
    def __init__(self, chunks: list[bytes], error: Exception | None = None) -> None:
        self._chunks = list(chunks)
        self._error = error

    def at_eof(self) -> bool:
        return not self._chunks

    async def read(self, n: int) -> bytes:
        if self._error is not None and not self._chunks:
            raise self._error
        return self._chunks.pop(0) if self._chunks else b""


def _as_process(proc: object) -> "asyncio.subprocess.Process":
    """假件冒充进程（`_drain_pipes` 只消费 stdout/stderr 两个属性）。"""
    return cast("asyncio.subprocess.Process", proc)


class _FakeProcess:
    def __init__(self, stdout: _FakeStream | None, stderr: _FakeStream | None) -> None:
        self.stdout = stdout
        self.stderr = stderr


class TestDrainPipes:
    async def test_collects_both_streams(self):
        proc = _FakeProcess(_FakeStream([b"out"]), _FakeStream([b"err"]))

        stdout, stderr = await _drain_pipes(_as_process(proc))

        assert (stdout, stderr) == (b"out", b"err")

    async def test_missing_streams_are_skipped(self):
        proc = _FakeProcess(None, None)

        assert await _drain_pipes(_as_process(proc)) == (b"", b"")

    async def test_read_error_ends_draining_quietly(self):
        """管道读取失败（对端已关）当作读完：已收到的字节仍要返回。"""
        proc = _FakeProcess(
            _FakeStream([b"first"], error=OSError("closed")), _FakeStream([])
        )

        stdout, stderr = await _drain_pipes(_as_process(proc))

        assert stdout == b"first" and stderr == b""

    async def test_slow_stream_times_out_without_raising(self):
        """整体等待有上限：排空不能变成新的挂起源。"""

        class _Never(_FakeStream):
            """永不到头的流：`read` 一直挂着，直到排空超时。"""

            async def read(self, n: int) -> bytes:
                await asyncio.sleep(60)
                return b""

        proc = _FakeProcess(_Never([]), _Never([]))

        assert await _drain_pipes(_as_process(proc), timeout=0.05) == (b"", b"")
