"""Write tool durability — tmp + os.replace (atomic replace), like Edit.

`write_file` used to `open(path, "w")` in place: a crash (or kill) mid-write
left a half-written file with the old content already truncated away, and
concurrent readers could observe partial content. The fix routes the payload
through `wing.common.fs.atomic_write_text` (tmp in the same directory + fsync
+ os.replace), matching Edit's semantics.

These tests lock the *mechanics*: the bytes reach the target through an
os.replace of a sibling tmp file, and a failure injected at any point before
the swap leaves the original file byte-identical (and no tmp litter behind).
The user-visible payload behavior (diff event shape, result strings) is locked
by test_diff_event.py and the probe scenario.
"""

from __future__ import annotations

import errno
import os
import stat
from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tools.builtin.write import write_file


class _StubCtx:
    """Minimal ToolContext stub (absolute paths, so cwd is unused)."""

    def __init__(self) -> None:
        self.session_id = "sess-write-test"
        self.cwd: str | None = None
        self.events: list = []

    def emit(self, event) -> None:
        self.events.append(event)


def _sibling_tmp_names(directory: Path) -> list[str]:
    return sorted(p.name for p in directory.iterdir() if ".tmp." in p.name)


class TestAtomicReplaceMechanics:
    @pytest.mark.asyncio
    async def test_target_is_replaced_from_sibling_tmp(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """Payload lands via os.replace(tmp, path); tmp is a sibling file."""
        target = tmp_path / "f.txt"
        target.write_text("old", encoding="utf-8")
        calls: list[tuple[str, str]] = []
        real_replace = os.replace

        def spy(src, dst):
            calls.append((str(src), str(dst)))
            return real_replace(src, dst)

        monkeypatch.setattr(os, "replace", spy)

        await write_file(str(target), "new", ctx=_StubCtx())  # type: ignore[arg-type]

        assert len(calls) == 1
        src, dst = calls[0]
        assert dst == str(target)
        # Same directory (same filesystem), distinct tmp name.
        assert Path(src).parent == tmp_path
        assert Path(src).name.startswith("f.txt.tmp.")
        assert Path(src).name != "f.txt"
        assert target.read_text(encoding="utf-8") == "new"
        assert _sibling_tmp_names(tmp_path) == []

    @pytest.mark.asyncio
    async def test_new_file_also_written_atomically(self, tmp_path: Path):
        target = tmp_path / "created.txt"
        await write_file(str(target), "hello", ctx=_StubCtx())  # type: ignore[arg-type]
        assert target.read_text(encoding="utf-8") == "hello"
        assert _sibling_tmp_names(tmp_path) == []

    @pytest.mark.asyncio
    async def test_parent_directories_created(self, tmp_path: Path):
        target = tmp_path / "deep" / "nested" / "f.txt"
        await write_file(str(target), "hello", ctx=_StubCtx())  # type: ignore[arg-type]
        assert target.read_text(encoding="utf-8") == "hello"


class TestFailureLeavesOriginalIntact:
    """Failure injected before the replace must not touch the target."""

    @pytest.mark.asyncio
    async def test_fsync_failure(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
        target = tmp_path / "f.txt"
        target.write_text("original", encoding="utf-8")

        def boom(_fd):
            raise OSError(errno.ENOSPC, "No space left on device")

        monkeypatch.setattr(os, "fsync", boom)

        with pytest.raises(ToolError, match="No space left on device"):
            await write_file(str(target), "half-written", ctx=_StubCtx())  # type: ignore[arg-type]

        assert target.read_text(encoding="utf-8") == "original"
        assert _sibling_tmp_names(tmp_path) == []  # tmp cleaned up

    @pytest.mark.asyncio
    async def test_replace_failure(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        target = tmp_path / "f.txt"
        target.write_text("original", encoding="utf-8")

        def boom(src, dst):
            raise OSError(errno.ENOSPC, "No space left on device")

        monkeypatch.setattr(os, "replace", boom)

        with pytest.raises(ToolError, match="No space left on device"):
            await write_file(str(target), "half-written", ctx=_StubCtx())  # type: ignore[arg-type]

        assert target.read_text(encoding="utf-8") == "original"
        assert _sibling_tmp_names(tmp_path) == []


class TestErrorClassification:
    """Error taxonomy is part of the tool contract (model reads the message)."""

    @pytest.mark.asyncio
    async def test_directory_target(self, tmp_path: Path):
        target = tmp_path / "adir"
        target.mkdir()
        with pytest.raises(ToolError, match="Is a directory"):
            await write_file(str(target), "x", ctx=_StubCtx())  # type: ignore[arg-type]
        assert target.is_dir()
        assert _sibling_tmp_names(tmp_path) == []  # no tmp litter next to the dir

    @pytest.mark.asyncio
    @pytest.mark.skipif(
        hasattr(os, "geteuid") and os.geteuid() == 0,
        reason="root bypasses directory permission bits",
    )
    async def test_permission_denied(self, tmp_path: Path):
        target = tmp_path / "f.txt"
        target.write_text("old", encoding="utf-8")
        target.chmod(0o400)
        directory = tmp_path
        directory.chmod(0o500)  # read+execute: tmp cannot be created
        try:
            with pytest.raises(ToolError, match="Permission denied"):
                await write_file(str(target), "x", ctx=_StubCtx())  # type: ignore[arg-type]
            assert target.read_text(encoding="utf-8") == "old"
        finally:
            directory.chmod(0o700)


class TestResultStrings:
    """Result strings stay byte-identical (models pattern-match on them)."""

    @pytest.mark.asyncio
    async def test_created(self, tmp_path: Path):
        result = await write_file(str(tmp_path / "new.txt"), "a\nb", ctx=_StubCtx())  # type: ignore[arg-type]
        assert result == "write: ok (created)\n  2 lines, 3 bytes"

    @pytest.mark.asyncio
    async def test_overwritten(self, tmp_path: Path):
        target = tmp_path / "f.txt"
        target.write_text("a\nb\nc\n", encoding="utf-8")
        result = await write_file(str(target), "x", ctx=_StubCtx())  # type: ignore[arg-type]
        assert result == "write: ok (overwritten)\n  3 → 1 lines, 1 bytes"


class TestTargetSemantics:
    """写入目标语义对齐就地写（open(path, "w")）的可见行为。"""

    @pytest.mark.asyncio
    async def test_overwrite_preserves_mode(self, tmp_path: Path):
        """覆盖既有文件保留其权限位（就地写语义；tmp+replace 会重置为 umask 默认）。"""
        for mode in (0o755, 0o600, 0o640):
            target = tmp_path / f"f-{mode:o}.txt"
            target.write_text("old", encoding="utf-8")
            target.chmod(mode)

            await write_file(str(target), "new", ctx=_StubCtx())  # type: ignore[arg-type]

            assert stat.S_IMODE(target.stat().st_mode) == mode, oct(
                stat.S_IMODE(target.stat().st_mode)
            )
            assert target.read_text(encoding="utf-8") == "new"

    @pytest.mark.asyncio
    @pytest.mark.skipif(
        hasattr(os, "geteuid") and os.geteuid() == 0,
        reason="root bypasses the target's read-only bit",
    )
    async def test_readonly_target_refused(self, tmp_path: Path):
        """只读目标拒绝写入（rename 只需要目录写权限，不加检查会绕过只读位）。"""
        target = tmp_path / "ro.txt"
        target.write_text("protected", encoding="utf-8")
        target.chmod(0o444)

        with pytest.raises(ToolError, match="Permission denied"):
            await write_file(str(target), "overwritten", ctx=_StubCtx())  # type: ignore[arg-type]

        assert target.read_text(encoding="utf-8") == "protected"
        assert stat.S_IMODE(target.stat().st_mode) == 0o444

    @pytest.mark.asyncio
    async def test_symlink_target_writes_through(self, tmp_path: Path):
        """末段是符号链接：写穿到真实目标，链接本身保留（就地写语义）。"""
        real = tmp_path / "real.txt"
        real.write_text("real", encoding="utf-8")
        link = tmp_path / "link.txt"
        link.symlink_to(real)

        await write_file(str(link), "through link", ctx=_StubCtx())  # type: ignore[arg-type]

        assert link.is_symlink()
        assert real.read_text(encoding="utf-8") == "through link"
        assert link.read_text(encoding="utf-8") == "through link"

    @pytest.mark.asyncio
    async def test_dangling_symlink_creates_the_target_file(self, tmp_path: Path):
        """悬空链接：目标文件被建出来（就地写顺着链接创建），链接保留。"""
        real = tmp_path / "missing.txt"
        link = tmp_path / "link.txt"
        link.symlink_to(real)
        assert not real.exists()

        await write_file(str(link), "created via link", ctx=_StubCtx())  # type: ignore[arg-type]

        assert link.is_symlink()
        assert real.read_text(encoding="utf-8") == "created via link"

    @pytest.mark.asyncio
    @pytest.mark.skipif(os.name != "posix", reason="/dev/null is a POSIX device node")
    async def test_special_file_written_in_place(self, tmp_path: Path):
        """设备节点等非常规文件：就地写（rename 会把节点本身换掉）。"""
        devnull = Path(os.devnull)
        before = os.stat(devnull)

        await write_file(str(devnull), "discarded", ctx=_StubCtx())  # type: ignore[arg-type]

        after = os.stat(devnull)
        assert stat.S_ISCHR(after.st_mode)  # 仍是字符设备
        assert (after.st_dev, after.st_ino) == (before.st_dev, before.st_ino)
        assert sorted(p.name for p in tmp_path.iterdir()) == []


class TestDiagnostics:
    """错误串是模型的自纠输入：保留就地写时代的可读诊断。"""

    @pytest.mark.asyncio
    async def test_parent_is_a_file_reports_not_a_directory(self, tmp_path: Path):
        """父路径组件是文件：报 "Not a directory"（不是 mkdir 的 "File exists"）。"""
        blocker = tmp_path / "blocker"
        blocker.write_text("x", encoding="utf-8")

        with pytest.raises(ToolError, match="Not a directory"):
            await write_file(str(blocker / "child.txt"), "y", ctx=_StubCtx())  # type: ignore[arg-type]

        assert blocker.read_text(encoding="utf-8") == "x"
