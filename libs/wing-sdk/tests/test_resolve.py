"""resolve_path 沙箱约束测试。"""

import os

import pytest

from wing_sdk.tools._resolve import resolve_path


def test_relative_path_inside(tmp_path):
    (tmp_path / "a.txt").write_text("x")
    resolved = resolve_path("a.txt", str(tmp_path))
    assert resolved == os.path.realpath(str(tmp_path / "a.txt"))


def test_nested_relative(tmp_path):
    (tmp_path / "sub").mkdir()
    resolved = resolve_path("sub/b.txt", str(tmp_path))
    assert resolved.startswith(os.path.realpath(str(tmp_path)) + os.sep)


def test_absolute_path_inside(tmp_path):
    f = tmp_path / "c.txt"
    f.write_text("x")
    assert resolve_path(str(f), str(tmp_path)) == os.path.realpath(str(f))


def test_relative_traversal_rejected(tmp_path):
    with pytest.raises(ValueError, match="escapes workspace"):
        resolve_path("../escape.txt", str(tmp_path))


def test_absolute_outside_rejected(tmp_path):
    with pytest.raises(ValueError, match="escapes workspace"):
        resolve_path("/etc/passwd", str(tmp_path))


def test_symlink_escape_rejected(tmp_path):
    ws = tmp_path / "ws"
    ws.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    link = ws / "link"
    os.symlink(outside, link)
    with pytest.raises(ValueError, match="escapes workspace"):
        resolve_path("link/secret.txt", str(ws))


def test_workspace_itself_allowed(tmp_path):
    assert resolve_path(".", str(tmp_path)) == os.path.realpath(str(tmp_path))


def test_prefix_sibling_not_confused(tmp_path):
    # /tmp/xxx-ws 不应被误判为 /tmp/xxx 的子路径（前缀攻击）
    ws = tmp_path / "ws"
    ws.mkdir()
    sibling = tmp_path / "ws-evil"
    sibling.mkdir()
    with pytest.raises(ValueError, match="escapes workspace"):
        resolve_path(str(sibling / "f.txt"), str(ws))
