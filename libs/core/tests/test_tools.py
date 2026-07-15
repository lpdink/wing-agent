"""Tests for Glob and Grep tools."""

from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tools import glob_files, grep_files


class TestGlobTool:
    """Test glob_files function."""

    @pytest.mark.asyncio
    async def test_glob_find_python_files(self, tmp_path: Path):
        """Test finding .py files."""
        # Create test files
        (tmp_path / "a.py").write_text("print('a')")
        (tmp_path / "b.py").write_text("print('b')")
        (tmp_path / "subdir").mkdir()
        (tmp_path / "subdir" / "c.py").write_text("print('c')")
        (tmp_path / "readme.md").write_text("# readme")

        result = await glob_files("**/*.py", str(tmp_path))
        assert "a.py" in result
        assert "b.py" in result
        assert "subdir/c.py" in result or "subdir\\c.py" in result
        assert "readme.md" not in result

    @pytest.mark.asyncio
    async def test_glob_no_matches(self, tmp_path: Path):
        """Test when no files match."""
        (tmp_path / "a.txt").write_text("hello")

        result = await glob_files("*.py", str(tmp_path))
        assert "no matches found" in result

    @pytest.mark.asyncio
    async def test_glob_invalid_path(self):
        """Test with non-existent directory."""
        with pytest.raises(ToolError) as exc_info:
            await glob_files("*.py", "/nonexistent/path")
        assert "No such directory" in str(exc_info.value)

    @pytest.mark.asyncio
    async def test_glob_nested_pattern(self, tmp_path: Path):
        """Test nested directory patterns."""
        (tmp_path / "src").mkdir()
        (tmp_path / "src" / "main.py").write_text("# main")
        (tmp_path / "src" / "utils").mkdir()
        (tmp_path / "src" / "utils" / "helper.py").write_text("# helper")

        result = await glob_files("src/**/*.py", str(tmp_path))
        assert "main.py" in result
        assert "helper.py" in result

    @pytest.mark.asyncio
    async def test_glob_no_gitignore_file(self, tmp_path: Path):
        """Test when .gitignore doesn't exist."""
        (tmp_path / "src").mkdir()
        (tmp_path / "src" / "main.py").write_text("# main")

        # Should work normally without gitignore
        result = await glob_files("**/*.py", str(tmp_path))
        assert "main.py" in result

    @pytest.mark.asyncio
    async def test_glob_doublestar_root(self, tmp_path: Path):
        """Test that '**' at root level matches files."""
        (tmp_path / "a.txt").write_text("a")
        (tmp_path / "subdir").mkdir()
        (tmp_path / "subdir" / "b.txt").write_text("b")

        # ** should match all files (fixed to **/*)
        result = await glob_files("**", str(tmp_path))
        assert "a.txt" in result
        assert "b.txt" in result

    @pytest.mark.asyncio
    async def test_glob_hidden_files_and_dirs(self, tmp_path: Path):
        """Test that hidden files and directories (dotfiles) are found."""
        # Hidden directory with files (like .github/workflows/)
        hidden_dir = tmp_path / ".hidden"
        hidden_dir.mkdir()
        (hidden_dir / "config.yml").write_text("key: value")

        # Hidden file at root
        (tmp_path / ".env").write_text("SECRET=1")

        # Normal file for comparison
        (tmp_path / "normal.txt").write_text("normal")

        result = await glob_files("**/*.yml", str(tmp_path))
        assert ".hidden/config.yml" in result or ".hidden\\config.yml" in result

        result_all = await glob_files("**", str(tmp_path))
        assert ".env" in result_all
        assert "normal.txt" in result_all


class TestGrepTool:
    """Test grep_files function."""

    @pytest.mark.asyncio
    async def test_grep_files_with_matches(self, tmp_path: Path):
        """Test finding files containing pattern."""
        (tmp_path / "a.py").write_text("def hello():\n    pass")
        (tmp_path / "b.py").write_text("def world():\n    pass")
        (tmp_path / "c.txt").write_text("hello world")

        result = await grep_files(
            "hello", str(tmp_path), output_mode="files_with_matches"
        )
        assert "a.py" in result
        assert "c.txt" in result
        assert "b.py" not in result

    @pytest.mark.asyncio
    async def test_grep_content_mode(self, tmp_path: Path):
        """Test content output mode."""
        (tmp_path / "test.py").write_text(
            "def foo():\n    return 'bar'\n\ndef baz():\n    pass"
        )

        result = await grep_files("def", str(tmp_path), output_mode="content")
        assert "test.py:1:" in result
        assert "test.py:4:" in result  # line 4, not 3 (line 3 is empty)
        assert "def foo" in result
        assert "def baz" in result

    @pytest.mark.asyncio
    async def test_grep_count_mode(self, tmp_path: Path):
        """Test count output mode."""
        (tmp_path / "test.py").write_text("hello\nhello\nworld")

        result = await grep_files("hello", str(tmp_path), output_mode="count")
        assert "test.py: 2" in result

    @pytest.mark.asyncio
    async def test_grep_case_insensitive(self, tmp_path: Path):
        """Test case insensitive search."""
        (tmp_path / "test.py").write_text("HELLO\nhello\nHello")

        result_sensitive = await grep_files(
            "hello", str(tmp_path), i=False, output_mode="count"
        )
        result_insensitive = await grep_files(
            "hello", str(tmp_path), i=True, output_mode="count"
        )

        assert "test.py: 1" in result_sensitive
        assert "test.py: 3" in result_insensitive

    @pytest.mark.asyncio
    async def test_grep_with_glob_filter(self, tmp_path: Path):
        """Test glob file filter."""
        (tmp_path / "a.py").write_text("TODO: fix this")
        (tmp_path / "b.txt").write_text("TODO: another")

        result = await grep_files(
            "TODO", str(tmp_path), glob="*.py", output_mode="files_with_matches"
        )
        assert "a.py" in result
        assert "b.txt" not in result

    @pytest.mark.asyncio
    async def test_grep_hidden_files(self, tmp_path: Path):
        """Test that grep searches hidden files and directories."""
        hidden_dir = tmp_path / ".config"
        hidden_dir.mkdir()
        (hidden_dir / "settings.yml").write_text("debug: true")
        (tmp_path / ".env").write_text("debug: true")
        (tmp_path / "normal.py").write_text("debug = True")

        result = await grep_files("debug", str(tmp_path))
        assert ".config/settings.yml" in result or ".config\\settings.yml" in result
        assert ".env" in result
        assert "normal.py" in result

    @pytest.mark.asyncio
    async def test_grep_no_matches(self, tmp_path: Path):
        """Test when no matches found."""
        (tmp_path / "a.py").write_text("hello world")

        result = await grep_files("nonexistent", str(tmp_path))
        assert "no matches found" in result

    @pytest.mark.asyncio
    async def test_grep_invalid_regex(self, tmp_path: Path):
        """Test with invalid regex pattern."""
        (tmp_path / "a.py").write_text("hello")

        with pytest.raises(ToolError) as exc_info:
            await grep_files("[invalid(", str(tmp_path))
        assert "invalid regex" in str(exc_info.value)

    @pytest.mark.asyncio
    async def test_grep_invalid_path(self):
        """Test with non-existent path."""
        with pytest.raises(ToolError) as exc_info:
            await grep_files("test", "/nonexistent/path")
        assert "No such file or directory" in str(exc_info.value)

    @pytest.mark.asyncio
    async def test_grep_head_limit(self, tmp_path: Path):
        """Test head_limit parameter."""
        for i in range(10):
            (tmp_path / f"file{i}.py").write_text("pattern")

        result = await grep_files("pattern", str(tmp_path), head_limit=3)
        # Should have 3 matches + truncation message
        lines = result.strip().split("\n")
        assert len([line for line in lines if ".py" in line]) == 3
        assert "truncated" in result

    @pytest.mark.asyncio
    async def test_grep_single_file(self, tmp_path: Path):
        """Test grep on a single file path (not directory).

        This is a bug fix: grep should work on file paths, not just directories.
        """
        # Create a single file
        (tmp_path / "test.py").write_text("class TestClass:\n    pass")

        # Grep on file path (glob is ignored for files)
        # files_with_matches mode returns just the filename
        result = await grep_files("class", str(tmp_path / "test.py"))
        assert "test.py" in result

        # Grep on file path with content mode - returns file:line:content
        result = await grep_files(
            "class", str(tmp_path / "test.py"), output_mode="content"
        )
        assert "TestClass" in result
        assert ":1:" in result  # line number

    @pytest.mark.asyncio
    async def test_grep_single_file_no_match(self, tmp_path: Path):
        """Test grep on file with no matches."""
        (tmp_path / "empty.py").write_text("# empty file")

        result = await grep_files("class", str(tmp_path / "empty.py"))
        assert "no matches found" in result

    @pytest.mark.asyncio
    async def test_grep_context_basic(self, tmp_path: Path):
        """Test context parameter shows surrounding lines."""
        (tmp_path / "test.py").write_text(
            "line 1\nline 2\nline 3\nMATCH HERE\nline 5\nline 6\nline 7"
        )

        result = await grep_files(
            "MATCH", str(tmp_path), output_mode="content", context=1
        )
        lines = result.strip().split("\n")
        # Should show: line 3, MATCH HERE, line 5 (1 line before and after)
        assert len(lines) == 3
        assert "line 3" in lines[0]
        assert "MATCH" in lines[1]
        assert "line 5" in lines[2]

    @pytest.mark.asyncio
    async def test_grep_context_overlapping_matches(self, tmp_path: Path):
        """Test that overlapping context ranges are merged."""
        (tmp_path / "test.py").write_text("line 1\nMATCH A\nline 3\nMATCH B\nline 5")

        result = await grep_files(
            "MATCH", str(tmp_path), output_mode="content", context=1
        )
        lines = result.strip().split("\n")
        # MATCH A at line 2 (context=1 → lines 1-3)
        # MATCH B at line 4 (context=1 → lines 3-5)
        # Ranges [0,3) and [2,5) overlap, merged into [0,5) = 5 lines
        assert len(lines) == 5
        assert all("--" not in line for line in lines)

    @pytest.mark.asyncio
    async def test_grep_context_separate_blocks(self, tmp_path: Path):
        """Test that non-adjacent match blocks are separated by --."""
        (tmp_path / "test.py").write_text(
            "MATCH A\nline 2\nline 3\nline 4\nline 5\nMATCH B\nline 7"
        )

        result = await grep_files(
            "MATCH", str(tmp_path), output_mode="content", context=1
        )
        # Two separate blocks with "--" between them
        assert "--" in result
        assert "MATCH A" in result
        assert "MATCH B" in result

    @pytest.mark.asyncio
    async def test_grep_context_zero(self, tmp_path: Path):
        """Test that context=0 behaves like no context (default behavior)."""
        (tmp_path / "test.py").write_text("line 1\nMATCH\nline 3")

        result_no_context = await grep_files(
            "MATCH", str(tmp_path), output_mode="content"
        )
        result_context_zero = await grep_files(
            "MATCH", str(tmp_path), output_mode="content", context=0
        )

        # Both should return only the matching line
        assert result_no_context == result_context_zero
        assert "MATCH" in result_context_zero
        assert "line 1" not in result_context_zero
        assert "line 3" not in result_context_zero

    @pytest.mark.asyncio
    async def test_grep_context_ignored_in_files_mode(self, tmp_path: Path):
        """Test that context is ignored in files_with_matches mode."""
        (tmp_path / "test.py").write_text("line 1\nMATCH\nline 3")

        result = await grep_files(
            "MATCH", str(tmp_path), output_mode="files_with_matches", context=10
        )
        # Should just return the filename, not context lines
        assert "test.py" in result
        assert "line 1" not in result

    @pytest.mark.asyncio
    async def test_grep_context_at_file_boundaries(self, tmp_path: Path):
        """Test context at file start and end boundaries."""
        (tmp_path / "test.py").write_text("MATCH\nline 2\nline 3")

        result = await grep_files(
            "MATCH", str(tmp_path), output_mode="content", context=5
        )
        # Should not error, just show available lines
        assert "MATCH" in result
        assert "line 2" in result
        assert "line 3" in result


class TestGlobGrepIntegration:
    """Integration tests for Glob and Grep together."""

    @pytest.mark.asyncio
    async def test_find_and_search(self, tmp_path: Path):
        """Test typical workflow: glob to find files, grep to search."""
        # Create project structure
        (tmp_path / "src").mkdir()
        (tmp_path / "src" / "main.py").write_text("import os\n\ndef main():\n    pass")
        (tmp_path / "src" / "utils.py").write_text("def helper():\n    return True")
        (tmp_path / "tests").mkdir()
        (tmp_path / "tests" / "test_main.py").write_text("def test_main():\n    pass")

        # Step 1: Find all Python files
        files_result = await glob_files("**/*.py", str(tmp_path))
        assert "main.py" in files_result
        assert "utils.py" in files_result
        assert "test_main.py" in files_result

        # Step 2: Search for 'def' in Python files
        # Note: glob="*.py" only matches root level, so we use ** pattern
        search_result = await grep_files(
            "def", str(tmp_path), glob="**/*.py", output_mode="content"
        )
        assert "def main" in search_result or "def helper" in search_result
