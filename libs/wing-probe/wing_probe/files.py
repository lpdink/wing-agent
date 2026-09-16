"""文件断言（spec probe-harness「Requirement: 文件断言」、tasks 5.4）。

面向 workspace 的断言面：存在 / 不存在 / 内容包含 / 内容相等 / 正则匹配 /
目录快照（相对路径集合 + 内容）。失败报告给出**路径与镜像**（期望 vs 实际，
长文本截断 + 首个差异行定位），便于"工具到底写了什么"的直接判读。

    files = FileAssertions(env.workspace)
    files.assert_content("notes/out.txt", contains="done")
    files.assert_missing("tmp/scratch.txt")
    files.assert_snapshot({"notes/out.txt": "done\\n"}, strict_paths=True)

路径口径：相对路径按 ``root`` 解析；绝对路径必须落在 ``root`` 内（越界即断言
失败——写错根目录是场景 bug，不是"无关紧要的差异"）。
"""

from __future__ import annotations

import fnmatch
import hashlib
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Final, Literal

from wing_probe.history.view import truncate


class FileAssertionError(AssertionError):
    """文件断言失败（报告含路径、期望、实际镜像）。"""


class _AnyContent:
    """``assert_snapshot`` 的哨兵：只校验存在性，不校验内容。"""

    __slots__ = ()

    def __repr__(self) -> str:
        return "ANY"


#: ``assert_snapshot`` 期望值：``str`` = 精确文本，``FileSnapshot`` = 摘要对账，
#: ``None`` = 必须不存在，``ANY`` = 只校验存在。
ANY: Final[_AnyContent] = _AnyContent()


@dataclass(frozen=True)
class FileSnapshot:
    """一个文件的快照（相对路径 + 尺寸 + sha256 + utf-8 文本）。"""

    path: str
    size: int
    sha256: str
    text: str | None
    """utf-8 解码文本；非 utf-8（二进制）为 None。"""

    def summary(self, *, limit: int = 120) -> str:
        kind = "binary" if self.text is None else f"{len(self.text)} char(s)"
        return (
            f"{self.path!r} ({self.size} byte(s), {kind}, sha256={self.sha256[:12]}…)"
        )


def _text_diff_hint(expected: str, actual: str, *, limit: int = 160) -> str:
    """首个差异行的定位（行号 + 期望/实际），否则给出尾部差异摘要。"""
    expected_lines = expected.splitlines()
    actual_lines = actual.splitlines()
    for index in range(max(len(expected_lines), len(actual_lines))):
        left = expected_lines[index] if index < len(expected_lines) else "<missing>"
        right = actual_lines[index] if index < len(actual_lines) else "<missing>"
        if left != right:
            return (
                f"first difference at line {index + 1}:\n"
                f"      expected: {truncate(left, limit)!r}\n"
                f"      actual  : {truncate(right, limit)!r}"
            )
    return (
        "texts match line by line but differ in raw bytes "
        f"(expected {len(expected)} char(s), actual {len(actual)} char(s))"
    )


class FileAssertions:
    """以 ``root`` 为边界的文件断言器（workspace 视图）。"""

    def __init__(self, root: Path | str) -> None:
        self.root: Path = Path(root).resolve()

    # ── 路径 ──────────────────────────────────────────────

    def resolve(self, path: Path | str) -> Path:
        """解析断言目标路径（绝对路径必须落在 root 内）。"""
        candidate = Path(path)
        resolved = (
            candidate.resolve()
            if candidate.is_absolute()
            else (self.root / candidate).resolve()
        )
        if resolved != self.root and self.root not in resolved.parents:
            raise FileAssertionError(
                f"path {str(path)!r} escapes the assertion root {self.root} "
                f"(resolved to {resolved})——场景断言的路径必须落在 workspace 内"
            )
        return resolved

    def relative(self, path: Path | str) -> str:
        """相对路径（posix 形式，报告用）。"""
        resolved = self.resolve(path)
        if resolved == self.root:
            return "."
        return resolved.relative_to(self.root).as_posix()

    # ── 存在性 ────────────────────────────────────────────

    def assert_exists(
        self, path: Path | str, *, kind: Literal["file", "dir"] | None = None
    ) -> Path:
        """断言路径存在（``kind`` 可进一步要求文件 / 目录）。"""
        resolved = self.resolve(path)
        if not resolved.exists():
            raise FileAssertionError(
                f"expected {self.relative(path)!r} to exist under {self.root}, "
                f"but it is missing "
                f"(resolved {resolved}; parent exists={resolved.parent.exists()})"
            )
        if kind == "file" and not resolved.is_file():
            raise FileAssertionError(
                f"expected {self.relative(path)!r} to be a file under {self.root}, "
                f"but it is a directory"
            )
        if kind == "dir" and not resolved.is_dir():
            raise FileAssertionError(
                f"expected {self.relative(path)!r} to be a directory under "
                f"{self.root}, but it is a file"
            )
        return resolved

    def assert_missing(self, path: Path | str) -> None:
        """断言路径不存在（文件或目录都算）。"""
        resolved = self.resolve(path)
        if resolved.exists():
            detail = (
                f"directory with {len(self.paths())} file(s)"
                if resolved.is_dir()
                else ""
            )
            if resolved.is_file():
                detail = f"file with {resolved.stat().st_size} byte(s)"
            raise FileAssertionError(
                f"expected {self.relative(path)!r} to be missing under {self.root}, "
                f"but it exists ({detail})"
            )

    # ── 读取 ──────────────────────────────────────────────

    def read_text(self, path: Path | str) -> str:
        """读取 utf-8 文本（缺失 / 解码失败抛 :class:`FileAssertionError`）。"""
        resolved = self.resolve(path)
        if not resolved.is_file():
            raise FileAssertionError(
                f"cannot read {self.relative(path)!r}: "
                f"{'not a file' if resolved.exists() else 'missing'} under {self.root}"
            )
        try:
            return resolved.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise FileAssertionError(
                f"cannot decode {self.relative(path)!r} as utf-8: {exc}"
            ) from exc

    # ── 内容 ──────────────────────────────────────────────

    def assert_content(
        self,
        path: Path | str,
        *,
        contains: str | None = None,
        equals: str | None = None,
        matches: str | re.Pattern[str] | None = None,
    ) -> None:
        """断言文件内容（``contains`` / ``equals`` / ``matches`` 三选一）。

        ``matches`` 用 ``re.search``（``re.MULTILINE``），失败报告给出模式与
        实际内容镜像。
        """
        modes = [
            name
            for name, value in (
                ("contains", contains),
                ("equals", equals),
                ("matches", matches),
            )
            if value is not None
        ]
        if len(modes) != 1:
            raise ValueError(
                "assert_content requires exactly one of contains=/equals=/matches= "
                f"(got {modes or 'none'})"
            )
        relative = self.relative(path)
        actual = self.read_text(path)
        header = f"{relative!r} under {self.root}"

        if equals is not None:
            if actual == equals:
                return
            raise FileAssertionError(
                f"content of {header} does not equal the expected text:\n"
                f"  expected ({len(equals)} char(s)): {truncate(equals, 200)!r}\n"
                f"  actual   ({len(actual)} char(s)): {truncate(actual, 200)!r}\n"
                f"  {_text_diff_hint(equals, actual)}"
            )
        if contains is not None:
            if contains in actual:
                return
            raise FileAssertionError(
                f"content of {header} does not contain the expected text:\n"
                f"  expected substring: {truncate(contains, 200)!r}\n"
                f"  actual ({len(actual)} char(s)): {truncate(actual, 200)!r}"
            )
        pattern = (
            matches
            if isinstance(matches, re.Pattern)
            else re.compile(str(matches), re.MULTILINE)
        )
        if pattern.search(actual) is not None:
            return
        raise FileAssertionError(
            f"content of {header} does not match the pattern:\n"
            f"  pattern: {pattern.pattern!r} (flags={pattern.flags})\n"
            f"  actual ({len(actual)} char(s)): {truncate(actual, 200)!r}"
        )

    # ── 快照 ──────────────────────────────────────────────

    def paths(self, *, ignore: Sequence[str] = ()) -> list[str]:
        """root 下全部文件（相对 posix 路径，字典序；``ignore`` 为 glob）。"""
        if not self.root.exists():
            return []
        result: list[str] = []
        for path in sorted(self.root.rglob("*")):
            if not path.is_file():
                continue
            relative = path.relative_to(self.root).as_posix()
            if any(fnmatch.fnmatch(relative, pattern) for pattern in ignore):
                continue
            result.append(relative)
        return result

    def snapshot(self, *, ignore: Sequence[str] = ()) -> dict[str, FileSnapshot]:
        """目录快照：``相对路径 → FileSnapshot``（含内容摘要）。"""
        result: dict[str, FileSnapshot] = {}
        for relative in self.paths(ignore=ignore):
            resolved = self.root / relative
            data = resolved.read_bytes()
            try:
                text: str | None = data.decode("utf-8")
            except UnicodeDecodeError:
                text = None
            result[relative] = FileSnapshot(
                path=relative,
                size=len(data),
                sha256=hashlib.sha256(data).hexdigest(),
                text=text,
            )
        return result

    def assert_snapshot(
        self,
        expected: Mapping[str, str | FileSnapshot | None | _AnyContent],
        *,
        ignore: Sequence[str] = (),
        strict_paths: bool = True,
    ) -> None:
        """目录快照对账：路径集合 + 内容。

        Args:
            expected: ``相对路径 → 期望``；``str`` = 精确文本，``FileSnapshot``
                = 尺寸 / sha256 对账（内容不一时给首个差异行），``None`` =
                必须不存在，``ANY`` = 只校验存在。
            ignore: 采集快照时忽略的 glob（构建 / 缓存目录等）。
            strict_paths: 为 True 时路径集合必须完全一致（多出的文件即违规）。

        Raises:
            FileAssertionError: 路径集合或内容不符。
        """
        actual = self.snapshot(ignore=ignore)
        problems: list[str] = []

        for relative in sorted(expected):
            want = expected[relative]
            if want is None:
                if relative in actual:
                    problems.append(
                        f"{relative!r}: expected to be missing, but it exists "
                        f"({actual[relative].summary()})"
                    )
                continue
            snapshot = actual.get(relative)
            if snapshot is None:
                problems.append(
                    f"{relative!r}: missing (expected {_expected_summary(want)})"
                )
                continue
            if isinstance(want, _AnyContent):
                continue
            if isinstance(want, FileSnapshot):
                problems.extend(_snapshot_diff(relative, want, snapshot))
                continue
            if snapshot.text is None:
                problems.append(
                    f"{relative!r}: expected utf-8 text content, but the file is "
                    f"not valid utf-8 ({snapshot.summary()})"
                )
                continue
            if snapshot.text != want:
                problems.append(
                    f"{relative!r}: content differs (expected {len(want)} char(s), "
                    f"actual {len(snapshot.text)} char(s)):\n"
                    f"      expected: {truncate(want, 200)!r}\n"
                    f"      actual  : {truncate(snapshot.text, 200)!r}\n"
                    f"      {_text_diff_hint(want, snapshot.text)}"
                )

        if strict_paths:
            for relative in sorted(set(actual) - set(expected)):
                problems.append(
                    f"{relative!r}: unexpected file "
                    f"({actual[relative].summary()})——快照要求路径集合完全一致"
                )

        if problems:
            body = "\n".join(f"  - {problem}" for problem in problems)
            listing = ", ".join(sorted(actual)) or "<empty>"
            raise FileAssertionError(
                f"file snapshot violated ({len(problems)} problem(s)) "
                f"[root {self.root}]:\n{body}\n"
                f"  actual paths: {listing}"
            )

    def describe(self, *, limit: int = 40) -> str:
        """目录摘要（失败报告素材 / 调试用）。"""
        entries = self.snapshot()
        lines = [f"workspace {self.root} ({len(entries)} file(s)):"]
        for relative in sorted(entries)[:limit]:
            lines.append(f"  {entries[relative].summary()}")
        if len(entries) > limit:
            lines.append(f"  … {len(entries) - limit} more file(s)")
        return "\n".join(lines)

    def __repr__(self) -> str:
        return f"FileAssertions(root={self.root})"


def _expected_summary(want: str | FileSnapshot | _AnyContent) -> str:
    if isinstance(want, _AnyContent):
        return "any content"
    if isinstance(want, FileSnapshot):
        return want.summary()
    return f"content {truncate(want, 120)!r}"


def _snapshot_diff(
    relative: str, want: FileSnapshot, actual: FileSnapshot
) -> list[str]:
    if want.sha256 == actual.sha256 and want.size == actual.size:
        return []
    problems = [
        f"{relative!r}: file differs (expected {want.size} byte(s) "
        f"sha256={want.sha256[:12]}…, actual {actual.size} byte(s) "
        f"sha256={actual.sha256[:12]}…)"
    ]
    if want.text is not None and actual.text is not None and want.text != actual.text:
        problems.append(f"{relative!r}: {_text_diff_hint(want.text, actual.text)}")
    return problems


__all__ = [
    "ANY",
    "FileAssertionError",
    "FileAssertions",
    "FileSnapshot",
]
