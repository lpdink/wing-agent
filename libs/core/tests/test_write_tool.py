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
