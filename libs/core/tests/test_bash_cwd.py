"""Tests for Bash execution and Glob/Grep cwd resolution.

Bash is stateless — no cwd persistence across calls. File/search tools
resolve relative paths against the session workspace directory.
"""

from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tools import glob_files, grep_files
from wing.tools.bash import execute_shell


class _MockAgent:
    """Minimal ToolContext implementation for tests."""

    def __init__(self, cwd: str | None = None) -> None:
        self._cwd = Path(cwd) if cwd else None
        self._yolo = True
        self._hooks: dict[str, object] = {}

    @property
    def session_id(self) -> str:
        return "test-session"

    @property
    def yolo(self) -> bool:
        return self._yolo

    @property
    def cwd(self) -> Path | None:
        return self._cwd

    def set_yolo(self, value: bool) -> None:
        self._yolo = value

    def set_cwd(self, path: Path | None) -> None:
        self._cwd = path

    def register_interrupt_hook(self, hook) -> str:
        import uuid

        hook_id = uuid.uuid4().hex
        self._hooks[hook_id] = hook
        return hook_id

    def unregister_interrupt_hook(self, hook_id: str) -> None:
        self._hooks.pop(hook_id, None)


def _parse_rc(result: str) -> int:
    """Extract the [exit code: N | Xs] prefix from a Bash result."""
    marker = "[exit code: "
    idx = result.index(marker) + len(marker)
    end = result.index("]", idx)
    # Format: "N | Xs" — extract just the exit code before " |"
    content = result[idx:end]
    rc_str = content.split(" |")[0] if " |" in content else content
    return int(rc_str)


class TestBashExecution:
    @pytest.mark.asyncio
    async def test_exit_code_is_honest(self, tmp_path: Path):
        """The returned exit code belongs to the user command."""
        agent = _MockAgent(str(tmp_path))

        assert _parse_rc(await execute_shell("true", agent, timeout=10)) == 0

        with pytest.raises(ToolError) as exc_info:
            await execute_shell("false", agent, timeout=10)
        assert _parse_rc(str(exc_info.value)) == 1

        with pytest.raises(ToolError) as exc_info:
            await execute_shell("exit 42", agent, timeout=10)
        assert _parse_rc(str(exc_info.value)) == 42

    @pytest.mark.asyncio
    async def test_cd_does_not_persist_across_calls(self, tmp_path: Path):
        """Bash is stateless — cd in one call does NOT affect the next."""
        sub = tmp_path / "subdir"
        sub.mkdir()
        agent = _MockAgent(str(tmp_path))

        await execute_shell("cd subdir", agent, timeout=10)

        result = await execute_shell("pwd", agent, timeout=10)
        # cwd remains the workspace root, not subdir
        assert str(sub) not in result

    @pytest.mark.asyncio
    async def test_timeout_returns_partial_output(self, tmp_path: Path):
        """A timed-out command returns partial output with error."""
        agent = _MockAgent(str(tmp_path))

        with pytest.raises(ToolError) as exc_info:
            await execute_shell("sleep 5", agent, timeout=1)
        assert "timed out" in str(exc_info.value)
        # cwd unchanged
        assert agent.cwd == tmp_path


class TestBashShellBehavior:
    """Basic shell features must work: multi-line, heredoc, background, pipe."""

    @pytest.mark.asyncio
    async def test_multiline_command(self, tmp_path: Path):
        agent = _MockAgent(str(tmp_path))
        result = await execute_shell("echo line1\necho line2", agent, timeout=10)
        assert "line1" in result and "line2" in result

    @pytest.mark.asyncio
    async def test_heredoc(self, tmp_path: Path):
        agent = _MockAgent(str(tmp_path))
        cmd = "cat <<EOF\nhello\nworld\nEOF"
        result = await execute_shell(cmd, agent, timeout=10)
        assert "hello" in result and "world" in result

    @pytest.mark.asyncio
    async def test_trailing_backslash(self, tmp_path: Path):
        """A trailing backslash should not corrupt the command."""
        agent = _MockAgent(str(tmp_path))
        result = await execute_shell("echo foo \\", agent, timeout=10)
        assert _parse_rc(result) == 0
        assert "foo" in result

    @pytest.mark.asyncio
    async def test_background_command_returns_immediately(self, tmp_path: Path):
        agent = _MockAgent(str(tmp_path))
        result = await execute_shell("sleep 2 &", agent, timeout=10)
        # Shell exits right after spawning the bg job — no timeout
        assert _parse_rc(result) == 0

    @pytest.mark.asyncio
    async def test_pipe_works(self, tmp_path: Path):
        agent = _MockAgent(str(tmp_path))
        result = await execute_shell("echo hello | tr a-z A-Z", agent, timeout=10)
        assert "HELLO" in result


class TestGlobGrepCwdResolution:
    """Glob/Grep resolve relative paths against ctx.cwd."""

    @pytest.mark.asyncio
    async def test_glob_resolves_relative_to_workspace(self, tmp_path: Path):
        (tmp_path / "deep").mkdir()
        (tmp_path / "deep" / "target.py").write_text("x = 1")
        (tmp_path / "other.py").write_text("y = 2")
        agent = _MockAgent(str(tmp_path))

        result = await glob_files("*.py", ctx=agent)
        assert "other.py" in result

    @pytest.mark.asyncio
    async def test_grep_resolves_relative_to_workspace(self, tmp_path: Path):
        (tmp_path / "src").mkdir()
        (tmp_path / "src" / "a.py").write_text("needle = 1\n")
        agent = _MockAgent(str(tmp_path))

        result = await grep_files("needle", path="src", ctx=agent)
        assert "a.py" in result
