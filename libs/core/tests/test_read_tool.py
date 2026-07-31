"""Tests for the Read tool (read_file) — Claude-aligned semantics."""

from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tools.file import MAX_LINES_TO_READ, read_file


@pytest.fixture
def sample_file(tmp_path: Path) -> Path:
    """Create a 10-line sample file."""
    p = tmp_path / "sample.py"
    p.write_text("\n".join(f"line {i}" for i in range(1, 11)))
    return p


@pytest.fixture
def long_file(tmp_path: Path) -> Path:
    """Create a file with 3000 lines."""
    p = tmp_path / "long.py"
    p.write_text("\n".join(f"L{i}" for i in range(1, 3001)))
    return p


class TestReadBasic:
    @pytest.mark.asyncio
    async def test_read_whole_file(self, sample_file: Path):
        result = await read_file(str(sample_file), ctx=None)  # type: ignore[arg-type]
        assert "lines 1-10/10" in result
        assert "line 1" in result
        assert "line 10" in result
        assert "more lines" not in result

    @pytest.mark.asyncio
    async def test_read_empty_file(self, tmp_path: Path):
        p = tmp_path / "empty.txt"
        p.write_text("")
        result = await read_file(str(p), ctx=None)  # type: ignore[arg-type]
        assert "empty" in result

    @pytest.mark.asyncio
    async def test_read_nonexistent(self):
        with pytest.raises(ToolError, match="No such file"):
            await read_file("/nonexistent/file.txt", ctx=None)  # type: ignore[arg-type]

    @pytest.mark.asyncio
    async def test_read_directory(self, tmp_path: Path):
        with pytest.raises(ToolError, match="Is a directory"):
            await read_file(str(tmp_path), ctx=None)  # type: ignore[arg-type]

    @pytest.mark.asyncio
    async def test_read_binary_file(self, tmp_path: Path):
        p = tmp_path / "bin.dat"
        p.write_bytes(b"\x00\x01\x02\x03")
        with pytest.raises(ToolError, match="Binary file"):
            await read_file(str(p), ctx=None)  # type: ignore[arg-type]


class TestReadOffset:
    """offset is 1-based: offset=1 starts at the first line."""

    @pytest.mark.asyncio
    async def test_offset_1_is_first_line(self, sample_file: Path):
        result = await read_file(str(sample_file), ctx=None, offset=1, limit=3)  # type: ignore[arg-type]
        assert "lines 1-3/10" in result
        assert "line 1" in result
        assert "line 3" in result
        assert "line 4" not in result

    @pytest.mark.asyncio
    async def test_offset_5_starts_at_fifth_line(self, sample_file: Path):
        result = await read_file(str(sample_file), ctx=None, offset=5, limit=2)  # type: ignore[arg-type]
        assert "lines 5-6/10" in result
        assert "line 5" in result
        assert "line 6" in result
        assert "line 4" not in result

    @pytest.mark.asyncio
    async def test_negative_offset_counts_from_end(self, sample_file: Path):
        result = await read_file(str(sample_file), ctx=None, offset=-3, limit=10)  # type: ignore[arg-type]
        # -3 means start 3 lines from end → line 8
        assert "lines 8-10/10" in result
        assert "line 8" in result
        assert "line 10" in result

    @pytest.mark.asyncio
    async def test_offset_beyond_file(self, sample_file: Path):
        result = await read_file(str(sample_file), ctx=None, offset=100, limit=5)  # type: ignore[arg-type]
        assert "beyond EOF" in result
        assert "10 lines" in result


class TestReadLimit:
    @pytest.mark.asyncio
    async def test_default_limit_2000(self, long_file: Path):
        result = await read_file(str(long_file), ctx=None)  # type: ignore[arg-type]
        assert f"lines 1-{MAX_LINES_TO_READ}/3000" in result
        assert f"[... {3000 - MAX_LINES_TO_READ} more lines]" in result

    @pytest.mark.asyncio
    async def test_limit_capped_at_max(self, long_file: Path):
        result = await read_file(str(long_file), ctx=None, limit=9999)  # type: ignore[arg-type]
        assert f"lines 1-{MAX_LINES_TO_READ}/3000" in result

    @pytest.mark.asyncio
    async def test_explicit_limit(self, sample_file: Path):
        result = await read_file(str(sample_file), ctx=None, limit=5)  # type: ignore[arg-type]
        assert "lines 1-5/10" in result
        assert "[... 5 more lines]" in result


class TestReadLineNumbers:
    @pytest.mark.asyncio
    async def test_line_numbers_off_by_default(self, sample_file: Path):
        import re

        result = await read_file(str(sample_file), ctx=None, limit=3)  # type: ignore[arg-type]
        # Content lines should not have N→ prefix
        content_lines = result.split("\n")[1:]  # skip header
        for line in content_lines:
            assert not re.match(r"^\d+→", line), (
                f"unexpected line-number prefix: {line!r}"
            )

    @pytest.mark.asyncio
    async def test_line_numbers_on(self, sample_file: Path):
        result = await read_file(
            str(sample_file), ctx=None, offset=3, limit=3, line_numbers=True
        )  # type: ignore[arg-type]
        lines = result.split("\n")
        # Skip header line
        assert lines[1] == "3→line 3"
        assert lines[2] == "4→line 4"
        assert lines[3] == "5→line 5"

    @pytest.mark.asyncio
    async def test_line_numbers_with_negative_offset(self, sample_file: Path):
        result = await read_file(
            str(sample_file), ctx=None, offset=-2, limit=5, line_numbers=True
        )  # type: ignore[arg-type]
        lines = result.split("\n")
        assert lines[1] == "9→line 9"
        assert lines[2] == "10→line 10"


class TestReadEncoding:
    @pytest.mark.asyncio
    async def test_utf8_bom_stripped(self, tmp_path: Path):
        p = tmp_path / "bom.txt"
        p.write_bytes(b"\xef\xbb\xbfhello")
        result = await read_file(str(p), ctx=None)  # type: ignore[arg-type]
        assert "utf-8" in result
        # BOM should be stripped by utf-8-sig decode
        assert "\ufeff" not in result
        assert "hello" in result

    @pytest.mark.asyncio
    async def test_latin1_fallback(self, tmp_path: Path):
        p = tmp_path / "latin.txt"
        p.write_bytes(b"caf\xe9")
        result = await read_file(str(p), ctx=None)  # type: ignore[arg-type]
        assert "latin-1" in result
