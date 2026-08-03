"""SendFile 路径安全解析单测。"""

import pytest

from wing_dingtalk.file_tool import _resolve_inside_root


def test_absolute_path_inside_root(tmp_path):
    target = tmp_path / "a.txt"
    target.write_text("x")
    assert _resolve_inside_root(tmp_path, str(target)) == target


def test_relative_path_resolves_under_root(tmp_path):
    target = tmp_path / "b.txt"
    target.write_text("x")
    assert _resolve_inside_root(tmp_path, "b.txt") == target


def test_dotdot_escape_rejected(tmp_path):
    with pytest.raises(ValueError, match="escapes"):
        _resolve_inside_root(tmp_path, "../evil.txt")


def test_missing_file_rejected(tmp_path):
    with pytest.raises(ValueError, match="not a file"):
        _resolve_inside_root(tmp_path, str(tmp_path / "nope.txt"))


def test_directory_rejected(tmp_path):
    sub = tmp_path / "sub"
    sub.mkdir()
    with pytest.raises(ValueError, match="not a file"):
        _resolve_inside_root(tmp_path, str(sub))
