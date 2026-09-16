"""文件断言单测（tasks 5.7）。

覆盖存在 / 不存在 / 内容包含 / 内容相等 / 正则匹配 / 目录快照（含路径集合口径）
的成功与失败路径；失败信息必须给出路径与期望 vs 实际镜像。
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from wing_probe.files import (
    ANY,
    FileAssertionError,
    FileAssertions,
    FileSnapshot,
)


@pytest.fixture()
def workspace(tmp_path: Path) -> Path:
    root = tmp_path / "ws"
    (root / "notes").mkdir(parents=True)
    (root / "notes" / "out.txt").write_text("done\nstep 2\n", encoding="utf-8")
    (root / "bin.dat").write_bytes(b"\x00\xff\x00")
    return root


@pytest.fixture()
def files(workspace: Path) -> FileAssertions:
    return FileAssertions(workspace)


# ── 存在 / 不存在 ───────────────────────────────────────────────


def test_assert_exists_success(files: FileAssertions) -> None:
    assert files.assert_exists("notes/out.txt").is_file()
    assert files.assert_exists("notes", kind="dir").is_dir()
    assert files.relative("notes/out.txt") == "notes/out.txt"


def test_assert_exists_reports_missing_path(files: FileAssertions) -> None:
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_exists("notes/nope.txt")
    text = str(excinfo.value)
    assert "expected 'notes/nope.txt' to exist" in text
    assert "it is missing" in text
    assert "parent exists=True" in text


def test_assert_exists_reports_wrong_kind(files: FileAssertions) -> None:
    with pytest.raises(FileAssertionError, match="but it is a directory"):
        files.assert_exists("notes", kind="file")
    with pytest.raises(FileAssertionError, match="but it is a file"):
        files.assert_exists("notes/out.txt", kind="dir")


def test_assert_missing_success_and_failure(files: FileAssertions) -> None:
    files.assert_missing("notes/gone.txt")
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_missing("notes/out.txt")
    assert "to be missing" in str(excinfo.value)
    assert "file with 12 byte(s)" in str(excinfo.value)
    with pytest.raises(FileAssertionError, match="it exists"):
        files.assert_missing("notes")


def test_path_must_stay_inside_root(files: FileAssertions, tmp_path: Path) -> None:
    with pytest.raises(FileAssertionError, match="escapes the assertion root"):
        files.assert_exists("../outside.txt")
    outside = tmp_path / "outside.txt"
    outside.write_text("x", encoding="utf-8")
    with pytest.raises(FileAssertionError, match="escapes the assertion root"):
        files.assert_exists(outside)
    # root 内的绝对路径照常可用
    files.assert_exists(files.root / "notes" / "out.txt")


# ── 内容 ────────────────────────────────────────────────────────


def test_assert_content_contains(files: FileAssertions) -> None:
    files.assert_content("notes/out.txt", contains="step 2")
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_content("notes/out.txt", contains="step 9")
    text = str(excinfo.value)
    assert "does not contain the expected text" in text
    assert "expected substring: 'step 9'" in text
    assert "'done\\nstep 2\\n'" in text


def test_assert_content_equals_with_diff_hint(files: FileAssertions) -> None:
    files.assert_content("notes/out.txt", equals="done\nstep 2\n")
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_content("notes/out.txt", equals="done\nstep 3\n")
    text = str(excinfo.value)
    assert "does not equal the expected text" in text
    assert "first difference at line 2" in text
    assert "expected: 'step 3'" in text
    assert "actual  : 'step 2'" in text


def test_assert_content_matches(files: FileAssertions) -> None:
    files.assert_content("notes/out.txt", matches=r"^step \d$")
    files.assert_content("notes/out.txt", matches=re.compile(r"done\nst"))
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_content("notes/out.txt", matches=r"^missing$")
    text = str(excinfo.value)
    assert "does not match the pattern" in text
    assert "pattern: '^missing$'" in text


def test_assert_content_requires_exactly_one_mode(files: FileAssertions) -> None:
    with pytest.raises(ValueError, match="exactly one of contains=/equals=/matches="):
        files.assert_content("notes/out.txt")
    with pytest.raises(ValueError, match="exactly one of contains=/equals=/matches="):
        files.assert_content("notes/out.txt", contains="done", equals="done")


def test_assert_content_reports_missing_file(files: FileAssertions) -> None:
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_content("notes/nope.txt", contains="x")
    assert "cannot read 'notes/nope.txt': missing" in str(excinfo.value)


def test_read_text_rejects_binary_file(files: FileAssertions) -> None:
    with pytest.raises(FileAssertionError, match="as utf-8"):
        files.read_text("bin.dat")


# ── 快照 ────────────────────────────────────────────────────────


def test_snapshot_records_paths_sizes_and_text(files: FileAssertions) -> None:
    snapshot = files.snapshot()
    assert sorted(snapshot) == ["bin.dat", "notes/out.txt"]
    text_file = snapshot["notes/out.txt"]
    assert text_file.text == "done\nstep 2\n"
    assert text_file.size == len("done\nstep 2\n")
    assert len(text_file.sha256) == 64
    assert snapshot["bin.dat"].text is None
    assert "binary" in snapshot["bin.dat"].summary()


def test_paths_sorted_and_ignore(files: FileAssertions) -> None:
    assert files.paths() == ["bin.dat", "notes/out.txt"]
    assert files.paths(ignore=["bin.*"]) == ["notes/out.txt"]
    assert files.paths(ignore=["notes/*"]) == ["bin.dat"]


def test_assert_snapshot_success(files: FileAssertions) -> None:
    files.assert_snapshot({"notes/out.txt": "done\nstep 2\n", "bin.dat": ANY})


def test_assert_snapshot_detects_missing_and_extra(files: FileAssertions) -> None:
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_snapshot({"notes/out.txt": ANY, "notes/never.txt": "x"})
    text = str(excinfo.value)
    assert "file snapshot violated" in text
    assert "'notes/never.txt': missing (expected content 'x')" in text
    assert "actual paths: bin.dat, notes/out.txt" in text


def test_assert_snapshot_detects_extra_path(files: FileAssertions) -> None:
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_snapshot({"notes/out.txt": ANY})
    assert "'bin.dat': unexpected file" in str(excinfo.value)
    # 非严格模式只对已列出的路径负责
    files.assert_snapshot({"notes/out.txt": ANY}, strict_paths=False)


def test_assert_snapshot_detects_content_drift(files: FileAssertions) -> None:
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_snapshot({"notes/out.txt": "done\nstep 9\n", "bin.dat": ANY})
    text = str(excinfo.value)
    assert "'notes/out.txt': content differs" in text
    assert "first difference at line 2" in text


def test_assert_snapshot_expected_missing_path(files: FileAssertions) -> None:
    files.assert_snapshot(
        {"notes/out.txt": ANY, "notes/gone.txt": None, "bin.dat": ANY}
    )
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_snapshot({"notes/out.txt": None, "bin.dat": ANY})
    text = str(excinfo.value)
    assert "'notes/out.txt': expected to be missing, but it exists" in text


def test_assert_snapshot_against_previous_snapshot(files: FileAssertions) -> None:
    before = files.snapshot()
    files.assert_snapshot(before, strict_paths=False)
    (files.root / "notes" / "out.txt").write_text("changed\n", encoding="utf-8")
    after = files.snapshot()
    with pytest.raises(FileAssertionError) as excinfo:
        files.assert_snapshot(before, strict_paths=False)
    text = str(excinfo.value)
    assert "file differs (expected 12 byte(s)" in text
    assert "notes/out.txt" in text
    # 新快照与自身一致
    files.assert_snapshot(after, strict_paths=False)


def test_snapshot_equality_helper(files: FileAssertions) -> None:
    snapshot = files.snapshot()
    assert isinstance(snapshot["notes/out.txt"], FileSnapshot)
    assert snapshot["notes/out.txt"] == files.snapshot()["notes/out.txt"]
    assert "workspace" in files.describe()
    assert "notes/out.txt" in files.describe()


def test_snapshot_ignore_patterns(files: FileAssertions) -> None:
    (files.root / ".cache").mkdir()
    (files.root / ".cache" / "junk.bin").write_bytes(b"junk")
    files.assert_snapshot(
        {".cache/junk.bin": ANY, "notes/out.txt": ANY, "bin.dat": ANY}
    )
    files.assert_snapshot({"notes/out.txt": ANY, "bin.dat": ANY}, ignore=[".cache/*"])
    assert files.snapshot(ignore=[".cache/*"]).keys() == {"bin.dat", "notes/out.txt"}
