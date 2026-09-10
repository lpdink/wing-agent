"""标准工具行为测试（Bash / Read / Write / Edit；Glob/Grep 依赖 rg）。"""

import os
import shutil

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
