"""标准工具行为测试（Bash / Read / Write / Edit；Glob/Grep 依赖 rg）。"""

import os
import shutil

from typing import cast

import asyncio

import pytest

from wing_sdk.tools.bash import bash
from wing_sdk.tools.file import edit, read, write
from wing_sdk.tools.search import glob, grep

pytestmark = pytest.mark.asyncio


# ── Bash ──────────────────────────────────────────────────────


async def test_bash_stdout_and_exit_code(tmp_path):
    out = await bash("echo hello", _workspace=str(tmp_path))
    assert "hello" in out
    assert "exit code: 0" in out


async def test_bash_nonzero_exit(tmp_path):
    out = await bash("exit 3", _workspace=str(tmp_path))
    assert "exit code: 3" in out


async def test_bash_stderr(tmp_path):
    out = await bash("echo oops >&2", _workspace=str(tmp_path))
    assert "[stderr]" in out
    assert "oops" in out


async def test_bash_cwd_is_workspace(tmp_path):
    out = await bash("pwd", _workspace=str(tmp_path))
    pwd_line = out.splitlines()[0]
    # macOS 下 /tmp 是 /private/tmp 的符号链接
    assert os.path.realpath(pwd_line) == os.path.realpath(str(tmp_path))


async def test_bash_timeout_kills_process_group(tmp_path):
    # 子进程（sleep 经 shell 派生）应随进程组一起被杀，不留孤儿
    out = await bash("sleep 30", timeout=1, _workspace=str(tmp_path))
    assert "timed out" in out
    # 确认 sleep 不在进程表中（killpg 生效）。
    # [s] 字符类避免 pgrep 匹配到自身父 shell 的命令行。
    check = await bash("pgrep -f 'sl[e]ep 30' || true", _workspace=str(tmp_path))
    assert "exit code: 0" in check
    pids = [ln for ln in check.splitlines() if ln.strip().isdigit()]
    assert pids == []


# ── Read / Write / Edit ───────────────────────────────────────


async def test_write_read_roundtrip(tmp_path):
    result = await write("a/b.txt", "line1\nline2\n", _workspace=str(tmp_path))
    assert "created" in result
    out = await read("a/b.txt", _workspace=str(tmp_path))
    assert "line1" in out and "line2" in out
    assert "| utf-8 |" in out  # header 含编码
    assert "mtime" in out  # header 含 mtime


async def test_write_overwrite(tmp_path):
    await write("f.txt", "old", _workspace=str(tmp_path))
    result = await write("f.txt", "new", _workspace=str(tmp_path))
    assert "overwritten" in result


async def test_read_offset_and_limit(tmp_path):
    await write(
        "f.txt", "\n".join(f"l{i}" for i in range(10)), _workspace=str(tmp_path)
    )
    out = await read("f.txt", offset=3, limit=2, _workspace=str(tmp_path))
    assert "lines 3-4/10" in out
    assert "l2" in out and "l3" in out
    assert "[... 6 more lines]" in out


async def test_read_negative_offset(tmp_path):
    await write("f.txt", "\n".join(f"l{i}" for i in range(5)), _workspace=str(tmp_path))
    out = await read("f.txt", offset=-1, _workspace=str(tmp_path))
    assert "lines 5-5/5" in out
    assert "l4" in out


async def test_read_line_numbers(tmp_path):
    await write("f.txt", "a\nb", _workspace=str(tmp_path))
    out = await read("f.txt", line_numbers=True, _workspace=str(tmp_path))
    assert "1→a" in out and "2→b" in out


async def test_read_empty_file(tmp_path):
    await write("f.txt", "", _workspace=str(tmp_path))
    out = await read("f.txt", _workspace=str(tmp_path))
    assert "empty" in out


async def test_read_missing_and_dir(tmp_path):
    assert "No such file" in await read("nope.txt", _workspace=str(tmp_path))
    assert "Is a directory" in await read(".", _workspace=str(tmp_path))


async def test_read_beyond_eof(tmp_path):
    await write("f.txt", "only", _workspace=str(tmp_path))
    out = await read("f.txt", offset=99, _workspace=str(tmp_path))
    assert "beyond EOF" in out


async def test_read_latin1_fallback(tmp_path):
    (tmp_path / "latin.txt").write_bytes(b"caf\xe9\n")
    out = await read("latin.txt", _workspace=str(tmp_path))
    assert "latin-1" in out
    assert "café" in out


async def test_read_binary_rejected(tmp_path):
    (tmp_path / "blob.png").write_bytes(b"\x89PNG\r\n\x00\x00data")
    out = await read("blob.png", _workspace=str(tmp_path))
    assert "Binary file" in out


async def test_edit_unique(tmp_path):
    await write("f.txt", "foo bar foo", _workspace=str(tmp_path))
    # Claude Code dialect: old_string / new_string (LLM-facing names).
    out = await edit(
        "f.txt", old_string="bar", new_string="baz", _workspace=str(tmp_path)
    )
    assert "ok @ line 1" in out
    assert "foo baz foo" in await read("f.txt", _workspace=str(tmp_path))


async def test_edit_spec_uses_claude_param_names():
    from wing_sdk.tools import _EDIT_PARAMS

    assert [p.name for p in _EDIT_PARAMS] == [
        "path",
        "old_string",
        "new_string",
        "replace_all",
    ]


async def test_edit_multi_requires_replace_all(tmp_path):
    await write("f.txt", "foo foo foo", _workspace=str(tmp_path))
    out = await edit("f.txt", "foo", "x", _workspace=str(tmp_path))
    assert "3 times" in out
    out = await edit("f.txt", "foo", "x", replace_all=True, _workspace=str(tmp_path))
    assert "3 replacements" in out


async def test_edit_not_found(tmp_path):
    await write("f.txt", "abc", _workspace=str(tmp_path))
    out = await edit("f.txt", "zzz", "x", _workspace=str(tmp_path))
    assert "not found" in out


async def test_tools_reject_escape(tmp_path):
    # 沙箱违规抛 ValueError（经 ToolHost 时转为 is_error 结果帧）
    with pytest.raises(ValueError, match="escapes workspace"):
        await write("../evil.txt", "x", _workspace=str(tmp_path))
    with pytest.raises(ValueError, match="escapes workspace"):
        await read("/etc/passwd", _workspace=str(tmp_path))


# ── Glob / Grep（需要 rg）──────────────────────────────────────

needs_rg = pytest.mark.skipif(
    shutil.which("rg") is None, reason="ripgrep not installed"
)


@needs_rg
async def test_glob_matches(tmp_path):
    await write("x/a.py", "", _workspace=str(tmp_path))
    await write("x/b.txt", "", _workspace=str(tmp_path))
    out = await glob("**/*.py", path="x", _workspace=str(tmp_path))
    assert "a.py" in out
    assert "b.txt" not in out


@needs_rg
async def test_grep_content_and_dash_pattern(tmp_path):
    await write("x/f.py", "hello world\n", _workspace=str(tmp_path))
    out = await grep("hello", path="x", output_mode="content", _workspace=str(tmp_path))
    assert "hello world" in out
    # 以 - 开头的 pattern 不应被当作 flag（-e 保护）
    await write("x/g.txt", "--flag line\n", _workspace=str(tmp_path))
    out = await grep("--flag", path="x", _workspace=str(tmp_path))
    assert "g.txt" in out


@needs_rg
async def test_grep_glob_filter(tmp_path):
    await write("x/a.py", "needle", _workspace=str(tmp_path))
    await write("x/a.txt", "needle", _workspace=str(tmp_path))
    out = await grep("needle", path="x", glob="*.py", _workspace=str(tmp_path))
    assert "a.py" in out
    assert "a.txt" not in out


# ── 错误路径：权限 / 目录 / 缺失 ───────────────────────────────

not_root = pytest.mark.skipif(
    hasattr(os, "geteuid") and os.geteuid() == 0,
    reason="root bypasses permission bits",
)


@not_root
async def test_read_permission_denied(tmp_path):
    target = tmp_path / "locked.txt"
    target.write_text("secret", encoding="utf-8")
    target.chmod(0o000)
    try:
        out = await read("locked.txt", _workspace=str(tmp_path))
        assert out == "read_file: locked.txt: Permission denied"
    finally:
        target.chmod(0o600)


@not_root
async def test_write_permission_denied(tmp_path):
    target = tmp_path / "f.txt"
    target.write_text("old", encoding="utf-8")
    target.chmod(0o400)
    try:
        out = await write("f.txt", "new", _workspace=str(tmp_path))
        assert out == "write: cannot write 'f.txt': Permission denied"
        assert target.read_text(encoding="utf-8") == "old"
    finally:
        target.chmod(0o600)


async def test_write_directory_target(tmp_path):
    (tmp_path / "adir").mkdir()
    out = await write("adir", "x", _workspace=str(tmp_path))
    assert out == "write: cannot write 'adir': Is a directory"


async def test_write_through_non_directory_component(tmp_path):
    # 中间段是文件 → NotADirectoryError（OSError 家族的统一兜底分支）
    await write("plain.txt", "x", _workspace=str(tmp_path))
    out = await write("plain.txt/child.txt", "x", _workspace=str(tmp_path))
    assert out.startswith("write: cannot write 'plain.txt/child.txt': ")


async def test_edit_missing_file(tmp_path):
    assert await edit("nope.txt", "a", "b", _workspace=str(tmp_path)) == (
        "edit: nope.txt: No such file"
    )


@not_root
async def test_edit_permission_denied(tmp_path):
    target = tmp_path / "locked.txt"
    target.write_text("abc", encoding="utf-8")
    target.chmod(0o000)
    try:
        assert await edit("locked.txt", "a", "b", _workspace=str(tmp_path)) == (
            "edit: locked.txt: Permission denied"
        )
    finally:
        target.chmod(0o600)


async def test_bash_reports_spawn_failure(tmp_path):
    # cwd 不存在 → create_subprocess_shell 直接抛错，工具须回结构化的 error 行
    out = await bash("echo hi", _workspace=str(tmp_path / "gone"))
    assert out.startswith("exit code: -1\nerror: ")


# ── Glob / Grep：边界分支 ─────────────────────────────────────


@needs_rg
async def test_glob_missing_directory(tmp_path):
    out = await glob("**/*.py", path="nope", _workspace=str(tmp_path))
    assert out == "glob: nope: No such directory"


@needs_rg
async def test_glob_no_matches(tmp_path):
    await write("x/a.py", "", _workspace=str(tmp_path))
    assert await glob("**/*.rs", path="x", _workspace=str(tmp_path)) == (
        "glob: no matches found"
    )


@needs_rg
async def test_glob_ignore_flag_plumbing(tmp_path, monkeypatch):
    """respect_gitignore 的唯一作用是给 rg 传 `--no-ignore`。

    这里测参数管线而不是「结果里有没有被 ignore 的文件」：rg 的 `--glob` 是白名单，
    匹配到的文件即使被 ignore 规则排除也会返回，所以对 Glob 来说该开关不改变结果。
    """
    import wing_sdk.tools.search as search_mod

    seen: list[list[str]] = []

    async def _spy(args, cwd):
        seen.append(list(args))
        return ("x/a.py", 0)

    monkeypatch.setattr(search_mod, "_run_rg", _spy)
    await write("a.py", "", _workspace=str(tmp_path))

    await glob("**/*.py", path=".", _workspace=str(tmp_path))
    assert "--no-ignore" not in seen[-1]

    await glob("**/*.py", path=".", respect_gitignore=False, _workspace=str(tmp_path))
    assert "--no-ignore" in seen[-1]


@needs_rg
async def test_glob_truncates_at_100_files(tmp_path):
    for i in range(105):
        await write(f"x/f{i:03}.py", "", _workspace=str(tmp_path))
    out = await glob("**/*.py", path="x", _workspace=str(tmp_path))
    lines = out.splitlines()
    assert len(lines) == 101  # 100 条 + 截断提示
    assert lines[-1] == "[... 5 more files]"


@needs_rg
async def test_glob_survives_stat_failure(tmp_path, monkeypatch):
    """stat 失败（NFS/权限）时退化为 rg 原始顺序，不能把结果吞掉。"""
    from pathlib import Path

    await write("x/a.py", "", _workspace=str(tmp_path))
    await write("x/b.py", "", _workspace=str(tmp_path))

    def _boom(self, **kwargs):
        raise OSError("stat unavailable")

    monkeypatch.setattr(Path, "stat", _boom)
    out = await glob("**/*.py", path="x", _workspace=str(tmp_path))
    assert sorted(out.splitlines()) == ["a.py", "b.py"]


@needs_rg
async def test_glob_sorts_by_mtime_newest_first(tmp_path):
    await write("x/old.py", "", _workspace=str(tmp_path))
    await write("x/new.py", "", _workspace=str(tmp_path))
    os.utime(tmp_path / "x" / "new.py", (2_000_000_000, 2_000_000_000))
    os.utime(tmp_path / "x" / "old.py", (1_000_000_000, 1_000_000_000))

    out = await glob("**/*.py", path="x", _workspace=str(tmp_path))
    assert out.splitlines()[0] == "new.py"


@needs_rg
async def test_grep_missing_path(tmp_path):
    out = await grep("x", path="nope", _workspace=str(tmp_path))
    assert out == "grep: nope: No such file or directory"


@needs_rg
async def test_grep_no_matches(tmp_path):
    await write("x/a.py", "alpha", _workspace=str(tmp_path))
    assert await grep("zzz", path="x", _workspace=str(tmp_path)) == (
        "grep: no matches found"
    )


@needs_rg
async def test_grep_file_target(tmp_path):
    """path 指向文件时以父目录为 cwd、文件名为 target（glob 参数被忽略）。"""
    await write("x/a.py", "needle\n", _workspace=str(tmp_path))
    out = await grep(
        "needle", path="x/a.py", output_mode="content", _workspace=str(tmp_path)
    )
    assert "needle" in out


@needs_rg
async def test_grep_case_insensitive_and_count(tmp_path):
    await write("x/a.py", "Needle\nneedle\n", _workspace=str(tmp_path))

    sensitive = await grep(
        "Needle", path="x", output_mode="count", _workspace=str(tmp_path)
    )
    assert "1" in sensitive

    insensitive = await grep(
        "needle", path="x", i=True, output_mode="count", _workspace=str(tmp_path)
    )
    assert "2" in insensitive


@needs_rg
async def test_grep_context_lines(tmp_path):
    await write("x/a.py", "l1\nl2\nneedle\nl4\n", _workspace=str(tmp_path))
    out = await grep(
        "needle",
        path="x",
        output_mode="content",
        context=1,
        _workspace=str(tmp_path),
    )
    assert "l2" in out and "l4" in out


@needs_rg
async def test_grep_respect_gitignore_toggle(tmp_path):
    await write("x/a.log", "needle", _workspace=str(tmp_path))
    await write("x/.ignore", "*.log\n", _workspace=str(tmp_path))

    assert await grep("needle", path="x", _workspace=str(tmp_path)) == (
        "grep: no matches found"
    )
    out = await grep(
        "needle", path="x", respect_gitignore=False, _workspace=str(tmp_path)
    )
    assert "a.log" in out


@needs_rg
async def test_grep_head_limit_truncation(tmp_path):
    await write(
        "x/a.py", "\n".join("needle" for _ in range(5)), _workspace=str(tmp_path)
    )
    out = await grep(
        "needle",
        path="x",
        output_mode="content",
        head_limit=2,
        _workspace=str(tmp_path),
    )
    assert out.splitlines()[-1] == "[... 3 more results]"


async def test_run_rg_without_ripgrep(monkeypatch):
    """环境无 rg：返回可读提示而不是抛 FileNotFoundError。"""
    import wing_sdk.tools.search as search_mod

    async def _no_rg(*args, **kwargs):
        raise FileNotFoundError("rg not found")

    monkeypatch.setattr(search_mod.asyncio, "create_subprocess_exec", _no_rg)
    assert await search_mod._run_rg(["--version"], cwd=".") == (
        "grep: ripgrep not available in this environment",
        1,
    )


async def test_run_rg_timeout_kills_process(monkeypatch):
    """超时必须杀进程组，否则 `rg` 会以孤儿身份留在进程表里。"""
    import asyncio

    import wing_sdk.tools.search as search_mod
    from wing_sdk.tools._proc import kill_process_group

    class _FakeProc:
        pid = 2_147_483_646  # 必然不存在的 pid：走 kill() 兜底分支
        killed = False

        async def communicate(self):
            return (b"", b"")

        def kill(self) -> None:
            self.killed = True

        async def wait(self) -> int:
            return 0

    proc = _FakeProc()

    async def _spawn(*args, **kwargs):
        return proc

    async def _timeout(coro, timeout=None):
        coro.close()
        raise asyncio.TimeoutError

    monkeypatch.setattr(search_mod.asyncio, "create_subprocess_exec", _spawn)
    monkeypatch.setattr(search_mod.asyncio, "wait_for", _timeout)

    assert await search_mod._run_rg(["--version"], cwd=".") == ("grep: timeout", 1)
    assert proc.killed is True
    # 兜底分支之外，真正的进程组击杀由 kill_process_group 自己的测试覆盖
    assert kill_process_group is not None


@needs_rg
async def test_kill_process_group_kills_real_tree(tmp_path):
    """真进程：killpg 后整个进程组消失（rg/bash 派生的子进程不会变孤儿）。"""
    import asyncio
    import signal

    from wing_sdk.tools._proc import kill_process_group

    proc = await asyncio.create_subprocess_exec("sleep", "30", start_new_session=True)
    try:
        kill_process_group(proc)
        assert await proc.wait() == -signal.SIGKILL
    finally:
        if proc.returncode is None:  # pragma: no cover - 清理兜底
            proc.kill()
            await proc.wait()


# ── 标准工具注册 ──────────────────────────────────────────────


async def test_register_standard_tools_binds_workspace(tmp_path):
    from wing_sdk.host import ToolHost
    from wing_sdk.tools import register_standard_tools

    host = ToolHost(client_id="h1")
    register_standard_tools(host, workspace=str(tmp_path))

    assert [s.name for s in host.specs] == [
        "Bash",
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
    ]
    # schema 与核心内置工具一致：参数名/类型经 to_dict 落到注册体
    read_spec = next(s for s in host.specs if s.name == "Read")
    assert [p.name for p in read_spec.params] == [
        "path",
        "offset",
        "limit",
        "line_numbers",
    ]
    assert read_spec.params[3].type == "boolean"

    # _workspace 已绑定：路径相对 workspace 解析
    await write("f.txt", "hello\n", _workspace=str(tmp_path))
    handler = host._tools["Read"].handler
    assert "hello" in await handler(path="f.txt")


async def test_kill_process_group_falls_back_when_proc_gone(monkeypatch):
    """pid 已退出且 kill() 也报 ProcessLookupError：静默吞掉，不向上抛。"""
    from wing_sdk.tools._proc import kill_process_group

    class _GoneProc:
        pid = 2_147_483_646

        def kill(self) -> None:
            raise ProcessLookupError("no such process")

    kill_process_group(cast("asyncio.subprocess.Process", _GoneProc()))
