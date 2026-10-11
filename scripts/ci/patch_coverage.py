#!/usr/bin/env python3
"""增量（patch）覆盖率：改动行 ∩ 可执行行 → 覆盖率 + 门禁。

口径：
- diff = ``git diff --unified=0 -M <merge-base(--base, HEAD)> HEAD`` 的新增行；
- Python = coverage.py JSON 的 ``executed_lines`` / ``missing_lines``（语句行；
  排除行不参与）；Rust = cargo-llvm-cov 的 LCOV（``DA`` 行记录，count 0 = 未执行）；
- 覆盖率 = 命中行 / 可执行行（**行级**，与总量统计的语句+分支/区域口径不同）。

**统计面** = 覆盖率产物自己覆盖的文件（Python 是 pyproject 的 ``source``，Rust 是
workspace crates）——只有面内文件的改动行参与判定。面外的代码（``scripts/``、
``libs/wing_hooks``、探针等）与**删除行**不判定，但会被点名（``unmeasured`` /
``removed_tests``）供评论展示：静默豁免会把「这块其实没人管」藏起来。

边界：diff 里没有可执行行（纯文档 / 配置 PR）→ 判过（``percent`` 为空）；覆盖率
产物缺失 → 不判失败、写 ``skipped`` 由评论说明；git 基线解析失败则**硬失败**
（配置错误必须可见，不能静默关掉门禁）。

判定与数字都写进 ``--out-json``：CI 评论按它渲染——展示的数字就是门禁用的数字。
阈值不在本文件声明：CI 经 ``make patch-cov-python / patch-cov-rust`` 传入
Makefile 的 ``PATCH_COV_MIN``（唯一事实来源）。

    python3 scripts/ci/patch_coverage.py --lang python \
        --coverage coverage-python.json --base origin/develop \
        --min 90.0 --out-json patch-python.json

``--selftest``：离线自检（diff / 两种覆盖率格式的解析、判定与 git 端到端），
不读仓库状态、不联网。
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Callable

#: hunk 头：`@@ -a[,b] +c[,d] @@ …`——只关心新文件侧的起始行。
_HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")


# ─── 数据结构 ────────────────────────────────────────────────────


@dataclass
class FilePatch:
    """单个文件在本次 diff 里的可执行改动行统计。"""

    path: str
    covered: int
    total: int
    missing: list[int] = field(default_factory=list)


@dataclass
class FileChange:
    """点名用的文件级计数：``lines`` 的含义随所属字段（新增行 / 净删除行）。"""

    path: str
    lines: int


@dataclass
class PatchResult:
    """一次判定的完整结果（``--out-json`` 的载荷）。"""

    lang: str
    base: str
    merge_base: str | None
    min: float
    covered: int = 0
    total: int = 0
    percent: float | None = None
    ok: bool = True
    skipped: str | None = None
    files: list[FilePatch] = field(default_factory=list)
    #: 统计面外的改动（非测试 .py / .rs 文件不在覆盖率产物里）：只点名，不判定。
    unmeasured: list[FileChange] = field(default_factory=list)
    #: 净删除的测试行（测试文件 removed - added > 0）：删除不进判定，只点名。
    removed_tests: list[FileChange] = field(default_factory=list)


# ─── 解析 ────────────────────────────────────────────────────────


def _unquote(path: str) -> str:
    """git 对含空格 / 非 ASCII 的路径输出 C 风格引号（``"b/a b.py"``、八进制转义的
    UTF-8 字节）——剥引号并解码，否则路径对不上覆盖率产物、整个文件静默逃出门禁。"""
    if len(path) >= 2 and path.startswith('"') and path.endswith('"'):
        raw = path[1:-1]
        try:
            return (
                raw.encode("latin-1", "backslashreplace")
                .decode("unicode_escape")
                .encode("latin-1")
                .decode("utf-8")
            )
        except UnicodeDecodeError:
            return raw
    return path


@dataclass
class DiffLines:
    """一次 diff 的行级改动：``added`` = {path: 新增行号}；``removed`` = {path: 删除行数}。"""

    added: dict[str, set[int]] = field(default_factory=dict)
    removed: dict[str, int] = field(default_factory=dict)


def _strip_prefix(target: str) -> str | None:
    """``b/x`` / ``a/x`` → ``x``；``/dev/null`` → None。"""
    if target == "/dev/null":
        return None
    if target.startswith(("b/", "a/")):
        return target[2:]
    return target


def parse_diff(diff_text: str) -> DiffLines:
    """unified diff（``-U0``）→ 新增行号与删除行数。

    hunk 状态机：``@@`` 进入 hunk；hunk 内只按 ``+`` / ``-`` / ``\\`` /
    上下文识别——以 ``++`` / ``--`` 开头的**内容行**（如新增一行 ``++ b/x``
    在 diff 里就是 ``+++ b/x``）因此不会被误当成 ``+++`` 文件头。

    新增行归 ``+++`` 侧路径、删除行归 ``---`` 侧路径：整文件删除时新侧是
    ``/dev/null``，删除行必须仍能归到旧路径上（否则「删掉整个测试文件」在
    ``removed`` 里消失）；改名带修改时两侧各归其位。
    """
    diff = DiffLines()
    add_path: str | None = None
    del_path: str | None = None
    cursor: int | None = None
    in_hunk = False
    for raw in diff_text.splitlines():
        if raw.startswith("@@"):
            matched = _HUNK.match(raw)
            if matched:
                cursor = int(matched.group(1))
                in_hunk = True
            continue
        if in_hunk:
            if raw.startswith("+"):
                if add_path is not None and cursor is not None:
                    diff.added.setdefault(add_path, set()).add(cursor)
                cursor = (cursor or 0) + 1
            elif raw.startswith("-"):
                if del_path is not None:
                    diff.removed[del_path] = diff.removed.get(del_path, 0) + 1
            elif raw.startswith("\\"):
                pass
            elif raw.startswith(" ") or raw == "":
                cursor = (cursor or 0) + 1  # 上下文行（-U0 下不出现；容忍）
            else:
                in_hunk = False  # hunk 结束：后续是文件级元数据
            continue
        if raw.startswith("+++ "):
            add_path = _strip_prefix(_unquote(raw[4:]))
        elif raw.startswith("--- "):
            del_path = _strip_prefix(_unquote(raw[4:]))
    return diff


def _is_python_test(path: str) -> bool:
    """Python 测试文件（按约定不在覆盖率统计面内）。"""
    name = Path(path).name
    return (
        "/tests/" in f"/{path}"
        or name.startswith("test_")
        or name.endswith("_test.py")
        or name == "conftest.py"
    )


def _is_rust_test(path: str) -> bool:
    """Rust 集成测试（``tests/`` 目录；源文件内的 ``#[cfg(test)]`` 无从按路径识别）。"""
    return "/tests/" in f"/{path}"


def _rel(path: str, root: Path) -> str:
    """覆盖率产物里的路径 → repo 相对路径（绝对路径按 root 归一，失败原样保留）。"""
    candidate = Path(path)
    if candidate.is_absolute():
        try:
            return candidate.relative_to(root).as_posix()
        except ValueError:
            return path
    return path


def parse_python_json(
    data: dict[str, Any], root: Path
) -> dict[str, tuple[set[int], set[int]]]:
    """coverage.py JSON → {repo 相对路径: (已执行行, 可执行行)}。"""
    out: dict[str, tuple[set[int], set[int]]] = {}
    files = data.get("files")
    if not isinstance(files, dict):
        return out
    for path, entry in files.items():
        if not isinstance(entry, dict):
            continue
        executed = {int(n) for n in entry.get("executed_lines") or []}
        missing = {int(n) for n in entry.get("missing_lines") or []}
        out[_rel(str(path), root)] = (executed, executed | missing)
    return out


def parse_lcov(text: str, root: Path) -> dict[str, dict[int, int]]:
    """LCOV → {repo 相对路径: {行: 命中次数}}（count 0 = 可执行但未命中）。"""
    out: dict[str, dict[int, int]] = {}
    current: str | None = None
    for raw in text.splitlines():
        if raw.startswith("SF:"):
            current = _rel(raw[3:].strip(), root)
            out.setdefault(current, {})
        elif raw.startswith("DA:") and current is not None:
            parts = raw[3:].split(",")
            if len(parts) >= 2:
                out[current][int(parts[0])] = int(parts[1])
        elif raw.startswith("end_of_record"):
            current = None
    return out


# ─── 判定 ────────────────────────────────────────────────────────


def compute_python(
    added: dict[str, set[int]],
    coverage: dict[str, tuple[set[int], set[int]]],
) -> list[FilePatch]:
    files: list[FilePatch] = []
    for path in sorted(added):
        entry = coverage.get(path)
        if entry is None:
            continue
        executed, coverable = entry
        considered = added[path] & coverable
        if not considered:
            continue
        hits = considered & executed
        files.append(
            FilePatch(
                path=path,
                covered=len(hits),
                total=len(considered),
                missing=sorted(considered - hits),
            )
        )
    return files


def compute_rust(
    added: dict[str, set[int]],
    coverage: dict[str, dict[int, int]],
) -> list[FilePatch]:
    files: list[FilePatch] = []
    for path in sorted(added):
        entry = coverage.get(path)
        if entry is None:
            continue
        considered = {line for line in added[path] if line in entry}
        if not considered:
            continue
        hits = {line for line in considered if entry[line] > 0}
        files.append(
            FilePatch(
                path=path,
                covered=len(hits),
                total=len(considered),
                missing=sorted(considered - hits),
            )
        )
    return files


def collect_unmeasured(
    added: dict[str, set[int]],
    coverage_paths: set[str],
    *,
    suffix: str,
    is_test: Callable[[str], bool],
) -> list[FileChange]:
    """统计面外的非测试代码改动（覆盖率产物里没有的文件）——只点名，不判定。

    测试文件剔除：它们按约定不在覆盖率统计面内（新测试行不判定是设计而非盲区）。
    """
    out = [
        FileChange(path=path, lines=len(added[path]))
        for path in added
        if path.endswith(suffix) and path not in coverage_paths and not is_test(path)
    ]
    return sorted(out, key=lambda f: (-f.lines, f.path))


def collect_removed_tests(
    diff: DiffLines, *, suffix: str, is_test: Callable[[str], bool]
) -> list[FileChange]:
    """净删除的测试行（removed - added > 0）——删除不进判定，只点名提醒人工确认。"""
    out = []
    for path, removed in diff.removed.items():
        if not path.endswith(suffix) or not is_test(path):
            continue
        net = removed - len(diff.added.get(path, ()))
        if net > 0:
            out.append(FileChange(path=path, lines=net))
    return sorted(out, key=lambda f: (-f.lines, f.path))


def judge(result: PatchResult) -> PatchResult:
    """汇总 files → covered/total/percent/ok。"""
    result.covered = sum(f.covered for f in result.files)
    result.total = sum(f.total for f in result.files)
    if result.skipped is not None:
        result.ok = True
        return result
    if result.total == 0:
        result.percent = None
        result.ok = True
        return result
    result.percent = result.covered * 100.0 / result.total
    result.ok = result.percent + 1e-9 >= result.min
    return result


# ─── git 与 CLI ──────────────────────────────────────────────────


def _git(root: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(root), *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return proc.stdout


def merge_base(root: Path, base: str) -> str:
    return _git(root, "merge-base", base, "HEAD").strip()


def diff_lines(root: Path, base: str) -> tuple[str, DiffLines]:
    """(merge_base, 改动行)——基线解析失败抛 CalledProcessError。"""
    mb = merge_base(root, base)
    text = _git(root, "diff", "--unified=0", "-M", mb, "HEAD")
    return mb, parse_diff(text)


def repo_root() -> Path:
    try:
        return Path(_git(Path.cwd(), "rev-parse", "--show-toplevel").strip())
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or "").strip() or str(error)
        raise SystemExit(f"patch-coverage: not inside a git repository ({detail})")


def run_gate(
    *,
    lang: str,
    coverage_path: Path,
    base: str,
    min_percent: float,
    root: Path,
) -> PatchResult:
    result = PatchResult(lang=lang, base=base, merge_base=None, min=min_percent)
    if not coverage_path.exists():
        result.skipped = f"coverage artifact not found: {coverage_path}"
        return judge(result)

    try:
        mb, diff = diff_lines(root, base)
        result.merge_base = mb
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or "").strip() or str(error)
        raise SystemExit(f"patch-coverage: cannot resolve base {base!r}: {detail}")

    if lang == "python":
        data = json.loads(coverage_path.read_text(encoding="utf-8"))
        coverage = parse_python_json(data, root)
        result.files = compute_python(diff.added, coverage)
        result.unmeasured = collect_unmeasured(
            diff.added,
            set(coverage),
            suffix=".py",
            is_test=_is_python_test,
        )
        result.removed_tests = collect_removed_tests(
            diff, suffix=".py", is_test=_is_python_test
        )
    else:
        coverage = parse_lcov(coverage_path.read_text(), root)
        result.files = compute_rust(diff.added, coverage)
        result.unmeasured = collect_unmeasured(
            diff.added,
            set(coverage),
            suffix=".rs",
            is_test=_is_rust_test,
        )
        result.removed_tests = collect_removed_tests(
            diff, suffix=".rs", is_test=_is_rust_test
        )
    return judge(result)


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="patch_coverage.py",
        description="Patch (incremental) coverage gate over a git diff and a coverage artifact.",
        epilog=(
            "example:\n"
            "  patch_coverage.py --lang rust --coverage coverage-rust.lcov \\\n"
            "      --base origin/develop --min 90.0 --out-json patch-rust.json\n"
            "  patch_coverage.py --selftest"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--lang", choices=["python", "rust"], help="coverage format")
    parser.add_argument("--coverage", metavar="PATH", help="coverage JSON / LCOV")
    parser.add_argument(
        "--base", metavar="REF", help="base branch ref (merge-base with HEAD)"
    )
    parser.add_argument(
        "--min", type=float, metavar="PCT", help="gate threshold, percent"
    )
    parser.add_argument("--out-json", metavar="PATH", help="write the verdict here")
    parser.add_argument(
        "--root", metavar="PATH", help="repo root (default: git toplevel)"
    )
    parser.add_argument(
        "--selftest", action="store_true", help="run offline self-checks"
    )
    return parser.parse_args(argv)


def format_report(result: PatchResult) -> str:
    lines = [
        f"patch coverage ({result.lang} vs {result.base} @ "
        f"{(result.merge_base or 'n/a')[:12]}): "
        + (
            f"{result.percent:.1f}% ({result.covered}/{result.total}), gate >= {result.min:.1f}% "
            f"-> {'PASS' if result.ok else 'FAIL'}"
            if result.percent is not None
            else result.skipped or "no coverable changed lines -> pass"
        )
    ]
    for file in result.files:
        if file.missing:
            lines.append(f"  {file.path}: missing {', '.join(map(str, file.missing))}")
    if result.unmeasured:
        lines.append(
            "  note: changed files outside the coverage surface (not judged): "
            + ", ".join(f"{f.path} ({f.lines})" for f in result.unmeasured[:10])
        )
    if result.removed_tests:
        lines.append(
            "  note: net-removed test lines (not judged; confirm by hand): "
            + ", ".join(f"{f.path} (-{f.lines})" for f in result.removed_tests[:10])
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.selftest:
        return _run_selftest()
    missing = [
        name
        for name in ("lang", "coverage", "base", "min")
        if getattr(args, name) is None
    ]
    if missing:
        print(
            f"patch_coverage: missing required --{'/--'.join(missing)}", file=sys.stderr
        )
        return 2

    root = Path(args.root) if args.root else repo_root()
    result = run_gate(
        lang=args.lang,
        coverage_path=Path(args.coverage),
        base=args.base,
        min_percent=args.min,
        root=root,
    )
    print(format_report(result))
    if args.out_json:
        Path(args.out_json).write_text(
            json.dumps(asdict(result), ensure_ascii=False, indent=1) + "\n",
            encoding="utf-8",
        )
    if result.skipped:
        print(f"patch_coverage: skipped ({result.skipped})", file=sys.stderr)
        return 0
    return 0 if result.ok else 1


# ─── 自检 ────────────────────────────────────────────────────────


def _run_selftest() -> int:
    """离线自检：解析、判定与 git 端到端（临时仓库，不读当前仓库状态）。"""
    failures: list[str] = []

    def check(name: str, condition: bool, detail: str = "") -> None:
        if condition:
            print(f"[selftest] ok   {name}")
        else:
            failures.append(name)
            print(f"[selftest] FAIL {name}: {detail}")

    diff = (
        "diff --git a/a.py b/a.py\n"
        "index 111..222 100644\n"
        "--- a/a.py\n"
        "+++ b/a.py\n"
        "@@ -1,3 +1,4 @@\n"
        " ctx0\n"
        "-old\n"
        "+new1\n"
        "+new2\n"
        " ctx2\n"
        "@@ -10,0 +14,2 @@\n"
        "+tail1\n"
        "+tail2\n"
        "@@ -20 +20 @@\n"
        "-removed\n"
        "+++ not-a-header\n"  # 内容行：新增的 `++ not-a-header`，diff 里正是 `+++ `
        "diff --git a/b.py b/b.py\n"
        "new file mode 100644\n"
        "--- /dev/null\n"
        "+++ b/b.py\n"
        "@@ -0,0 +1,2 @@\n"
        "+first\n"
        "+second\n"
        'diff --git "a/with space.py" "b/with space.py"\n'
        '--- "a/with space.py"\n'
        '+++ "b/with space.py"\n'
        "@@ -0,0 +1 @@\n"
        "+spaced\n"
        'diff --git a/x.py "b/\\346\\226\\207.py"\n'
        "--- a/x.py\n"
        '+++ "b/\\346\\226\\207.py"\n'
        "@@ -1 +1 @@\n"
        "-old\n"
        "+new\n"
        "diff --git a/t.py b/t.py\n"
        "--- a/t.py\n"
        "+++ b/t.py\n"
        "@@ -1 +1 @@\n"
        "-a\n"
        "-b\n"
        "-c\n"
        "+keep\n"
        "diff --git a/tests/test_gone.py b/tests/test_gone.py\n"
        "deleted file mode 100644\n"
        "--- a/tests/test_gone.py\n"
        "+++ /dev/null\n"
        "@@ -1,3 +0,0 @@\n"
        "-a\n"
        "-b\n"
        "-c\n"
        "diff --git a/tests/test_old.py b/tests/test_new.py\n"
        "similarity index 80%\n"
        "rename from tests/test_old.py\n"
        "rename to tests/test_new.py\n"
        "--- a/tests/test_old.py\n"
        "+++ b/tests/test_new.py\n"
        "@@ -1,2 +1,2 @@\n"
        "-stale\n"
        "+fresh\n"
    )
    parsed = parse_diff(diff)
    added, removed = parsed.added, parsed.removed
    check(
        "diff: hunk 行号与上下文/删除行",
        added["a.py"] == {2, 3, 14, 15, 20},
        repr(added),
    )
    check("diff: 内容行不撞文件头", "not-a-header" not in added, repr(added))
    check("diff: 新文件", added["b.py"] == {1, 2}, repr(added))
    check(
        "diff: 引号路径（空格 / 八进制 UTF-8）",
        added.get("with space.py") == {1} and added.get("文.py") == {1},
        repr(added),
    )
    check(
        "diff: 删除行计数",
        removed.get("a.py") == 2 and removed.get("t.py") == 3,
        repr(removed),
    )
    check(
        "diff: 整文件删除的删除行归旧路径",
        removed.get("tests/test_gone.py") == 3,
        repr(removed),
    )
    check(
        "diff: 改名带修改两侧各归其位",
        removed.get("tests/test_old.py") == 1 and added.get("tests/test_new.py") == {1},
        repr((added, removed)),
    )

    root = Path("/repo")
    py = parse_python_json(
        {
            "files": {
                "/repo/x.py": {
                    "executed_lines": [1, 2],
                    "missing_lines": [3],
                    "excluded_lines": [4],
                },
                "y.py": {"executed_lines": [], "missing_lines": [5]},
            }
        },
        root,
    )
    check("python: 绝对路径归一", "x.py" in py and "/repo/x.py" not in py, repr(py))
    check("python: executed+missing", py["x.py"] == ({1, 2}, {1, 2, 3}), repr(py))

    lcov = (
        "SF:/repo/src/lib.rs\n"
        "DA:1,1\n"
        "DA:2,0\n"
        "DA:3,7,checksum\n"
        "end_of_record\n"
        "SF:other.rs\n"
        "DA:9,0\n"
        "end_of_record\n"
    )
    rs = parse_lcov(lcov, root)
    check("lcov: 解析与归一", rs["src/lib.rs"] == {1: 1, 2: 0, 3: 7}, repr(rs))

    files = compute_python(
        {"x.py": {1, 2, 3, 4, 5}, "nope.py": {1}}, {"x.py": ({1, 2}, {1, 2, 3})}
    )
    check(
        "python: 只数可执行改动行",
        files == [FilePatch("x.py", covered=2, total=3, missing=[3])],
        repr(files),
    )
    files = compute_rust({"src/lib.rs": {1, 2, 3}}, {"src/lib.rs": {1: 1, 2: 0, 99: 1}})
    check(
        "rust: count 0 计未命中",
        files == [FilePatch("src/lib.rs", covered=1, total=2, missing=[2])],
        repr(files),
    )

    result = judge(
        PatchResult(lang="python", base="origin/develop", merge_base=None, min=90.0)
    )
    check(
        "judge: 空 patch 不判失败", result.ok and result.percent is None, repr(result)
    )
    result = judge(
        PatchResult(
            lang="python",
            base="origin/develop",
            merge_base=None,
            min=90.0,
            files=[FilePatch("x.py", covered=1, total=4, missing=[2, 3, 4])],
        )
    )
    check("judge: 低于阈值失败", not result.ok and result.percent == 25.0, repr(result))
    result = judge(
        PatchResult(
            lang="rust",
            base="origin/develop",
            merge_base=None,
            min=90.0,
            skipped="coverage artifact not found: x",
        )
    )
    check(
        "judge: 数据缺失不判失败",
        result.ok and result.skipped is not None,
        repr(result),
    )

    unmeasured = collect_unmeasured(
        {
            "libs/core/wing/a.py": {1, 2},
            "scripts/tool.py": {1},
            "libs/core/tests/test_a.py": {1},
        },
        {"libs/core/wing/a.py"},
        suffix=".py",
        is_test=_is_python_test,
    )
    check(
        "blind spot: 统计面外点名（测试剔除）",
        unmeasured == [FileChange("scripts/tool.py", 1)],
        repr(unmeasured),
    )
    removed_tests = collect_removed_tests(
        parse_diff(
            "diff --git a/tests/test_a.py b/tests/test_a.py\n"
            "--- a/tests/test_a.py\n"
            "+++ b/tests/test_a.py\n"
            "@@ -1,5 +1,2 @@\n"
            "-one\n"
            "-two\n"
            "-three\n"
            "+kept\n"
            "+kept2\n"
            "diff --git a/tests/test_gone.py b/tests/test_gone.py\n"
            "deleted file mode 100644\n"
            "--- a/tests/test_gone.py\n"
            "+++ /dev/null\n"
            "@@ -1,2 +0,0 @@\n"
            "-gone1\n"
            "-gone2\n"
        ),
        suffix=".py",
        is_test=_is_python_test,
    )
    check(
        "blind spot: 净删除测试行点名（含整文件删除）",
        removed_tests
        == [FileChange("tests/test_gone.py", 2), FileChange("tests/test_a.py", 1)],
        repr(removed_tests),
    )

    with tempfile.TemporaryDirectory(prefix="wing-patch-cov-") as tmp:
        repo = Path(tmp)
        _git(repo, "init", "-q")
        _git(repo, "config", "user.email", "selftest@invalid")
        _git(repo, "config", "user.name", "selftest")
        module = repo / "x.py"
        module.write_text("a = 1\nb = 2\n", encoding="utf-8")
        _git(repo, "add", "x.py")
        _git(repo, "commit", "-q", "-m", "base")
        module.write_text("a = 1\nb = 2\nc = 3\n", encoding="utf-8")
        _git(repo, "add", "x.py")
        _git(repo, "commit", "-q", "-m", "change")
        coverage_file = repo / "cov.json"
        coverage_file.write_text(
            json.dumps(
                {"files": {"x.py": {"executed_lines": [3], "missing_lines": []}}}
            ),
            encoding="utf-8",
        )
        outcome = run_gate(
            lang="python",
            coverage_path=coverage_file,
            base="HEAD~1",
            min_percent=90.0,
            root=repo,
        )
        check(
            "git e2e: 改动行判定",
            outcome.ok
            and outcome.covered == 1
            and outcome.total == 1
            and outcome.merge_base,
            repr(outcome),
        )

    print(
        f"[selftest] {'all passed' if not failures else 'failed: ' + ', '.join(failures)}"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
