#!/usr/bin/env python3
"""Python / Rust 覆盖率数据 → PR 评论 markdown。

ci.yml 的 python-check / rust-check 两个 job 各产出一份机器可读覆盖率产物
（coverage json / llvm-cov export）与一份**增量覆盖率判定**（patch json，
`scripts/ci/patch_coverage.py` 写出的原样载荷）；本脚本合并渲染成**一条**
sticky 评论；`sticky_comment.py` 按首行的 `<!-- wing-coverage -->` marker
查找并原地更新。

门禁与**判定数字**都不在这里重复声明：增量覆盖率取两个 patch json（门禁用的
就是它们），阈值取 Makefile 的 `PATCH_COV_MIN`（门禁的唯一事实来源）。任何
数据源缺失时，对应行显示 `—` 并在「注意」里说明，评论照发 —— 覆盖率没到或
测试挂了，恰恰是最需要看到这条评论的时候。

    python3 scripts/ci/coverage_report.py \
        --python-json coverage-python.json --rust-json coverage-rust.json \
        --patch-python patch-python.json --patch-rust patch-rust.json \
        --out-md comment-body.md --run-url https://github.com/.../actions/runs/123
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]

#: 评论唯一标识：sticky_comment.py 据此查找 / 更新同一条评论。
MARKER = "<!-- wing-coverage -->"

#: 单文件最多列出的未覆盖行条数（超出以 `…` 收尾，避免大 PR 把评论撑爆）。
MAX_MISSING_ENTRIES = 20

#: 未覆盖行的折叠块最多列出的文件数（超出只计数，避免大 PR 把评论撑爆）。
MAX_MISSING_FILES = 30


def _read_json(path: str) -> dict[str, Any] | None:
    """读一份 JSON 产物；读不出（缺失 / 坏 JSON）返回 None，由调用方走「数据缺失」分支。"""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"warning: cannot read {path}: {error}", file=sys.stderr)
        return None
    return data if isinstance(data, dict) else None


def load_patch_min() -> float | None:
    """Makefile 的 PATCH_COV_MIN → 增量覆盖率阈值（门禁与评论的唯一事实来源）。"""
    try:
        text = (REPO_ROOT / "Makefile").read_text(encoding="utf-8")
    except OSError:
        return None
    matched = re.search(r"^PATCH_COV_MIN\s*[:?]?=\s*([0-9.]+)", text, re.M)
    return float(matched.group(1)) if matched else None


def python_totals(data: dict[str, Any]) -> tuple[float, int, int] | None:
    """Python 总计 → (覆盖率, 语句数, 分支数)。

    口径与 `coverage report` 的 Cover 列一致：语句 + 分支合并
    （(covered_lines + covered_branches) / (statements + branches)）；
    `percent_covered` 已是该口径，这里只在聚合分包时重算。
    """
    totals = data.get("totals")
    if not isinstance(totals, dict) or "percent_covered" not in totals:
        return None
    percent = totals.get("percent_covered")
    if not isinstance(percent, int | float):
        return None
    return (
        float(percent),
        int(totals.get("num_statements", 0)),
        int(totals.get("num_branches", 0)),
    )


def python_packages(data: dict[str, Any]) -> list[tuple[str, float, int, int]]:
    """按包聚合（wing / wing_sdk）；返回 [(包名, 覆盖率, 语句数, 分支数)]，覆盖率升序。"""
    files = data.get("files")
    if not isinstance(files, dict):
        return []
    agg: dict[str, list[int]] = {}
    for path, entry in files.items():
        parts = Path(path).parts
        if "wing_sdk" in parts:
            package = "wing_sdk"
        elif "wing" in parts:
            package = "wing"
        else:
            continue
        summary = entry.get("summary", {}) if isinstance(entry, dict) else {}
        numbers = agg.setdefault(package, [0, 0, 0, 0])
        numbers[0] += int(summary.get("covered_lines", 0))
        numbers[1] += int(summary.get("num_statements", 0))
        numbers[2] += int(summary.get("covered_branches", 0))
        numbers[3] += int(summary.get("num_branches", 0))
    rows = []
    for package, (cov_lines, stmts, cov_branches, branches) in agg.items():
        total = stmts + branches
        percent = (cov_lines + cov_branches) * 100.0 / total if total else 0.0
        rows.append((package, percent, stmts, branches))
    return sorted(rows, key=lambda row: row[1])


def rust_totals(data: dict[str, Any]) -> tuple[float, float] | None:
    """Rust 总计 → (行覆盖率, 区域覆盖率)。"""
    try:
        totals = data["data"][0]["totals"]
        return float(totals["lines"]["percent"]), float(totals["regions"]["percent"])
    except (KeyError, IndexError, TypeError, ValueError):
        return None


def rust_crates(data: dict[str, Any]) -> list[tuple[str, float, float]]:
    """按 crate 聚合；返回 [(crate, 行覆盖率, 区域覆盖率)]，行覆盖率升序。"""
    try:
        files = data["data"][0]["files"]
    except (KeyError, IndexError, TypeError):
        return []
    agg: dict[str, list[int]] = {}
    for entry in files:
        if not isinstance(entry, dict):
            continue
        matched = re.search(r"/crates/([^/]+)/", str(entry.get("filename", "")))
        if not matched:
            continue
        summary = entry.get("summary", {})
        numbers = agg.setdefault(matched.group(1), [0, 0, 0, 0])
        numbers[0] += int(summary.get("lines", {}).get("covered", 0))
        numbers[1] += int(summary.get("lines", {}).get("count", 0))
        numbers[2] += int(summary.get("regions", {}).get("covered", 0))
        numbers[3] += int(summary.get("regions", {}).get("count", 0))
    rows = []
    for crate, (cov_lines, lines, cov_regions, regions) in agg.items():
        line_pct = cov_lines * 100.0 / lines if lines else 0.0
        region_pct = cov_regions * 100.0 / regions if regions else 0.0
        rows.append((crate, line_pct, region_pct))
    return sorted(rows, key=lambda row: row[1])


def _fmt(percent: float | None, digits: int) -> str:
    return f"{percent:.{digits}f}%" if percent is not None else "—"


def _fmt_lines(lines: list[int]) -> str:
    """[12, 13, 14, 33] → "12-14, 33"（连续段压缩；超长截断）。"""
    entries: list[str] = []
    start = prev = None
    for line in lines:
        if start is None:
            start = prev = line
        elif line == prev + 1:
            prev = line
        else:
            entries.append(f"{start}" if start == prev else f"{start}-{prev}")
            start = prev = line
    if start is not None:
        entries.append(f"{start}" if start == prev else f"{start}-{prev}")
    if len(entries) > MAX_MISSING_ENTRIES:
        kept = entries[:MAX_MISSING_ENTRIES]
        return ", ".join(kept) + ", …"
    return ", ".join(entries)


def _patch_row(lang: str, patch: dict[str, Any] | None, threshold: float | None) -> str:
    """增量覆盖率表格的一行：语言 | 改动行覆盖 | 覆盖/可执行 | 门禁 | 状态。

    无改动可执行行 = 判过（`✅ 无改动行`）；产物缺失 = 未判定（原因进「注意」区）。
    """
    gate = _fmt(threshold, 1)
    if patch is None:
        return f"| {lang} | — | — | ≥ {gate} | — |"
    percent = patch.get("percent")
    covered, total = patch.get("covered"), patch.get("total")
    if percent is None:
        # 无改动可执行行（纯文档 / 配置 PR）是**判过**；产物缺失才是未判定（原因进「注意」区）。
        status = "⏭️ 未判定" if patch.get("skipped") else "✅ 无改动行"
        return f"| {lang} | — | — | ≥ {gate} | {status} |"
    status = "✅" if patch.get("ok") else "❌"
    return (
        f"| {lang} | {_fmt(float(percent), 1)} | {covered} / {total} "
        f"| ≥ {gate} | {status} |"
    )


def _patch_missing_details(
    patch_python: dict[str, Any] | None, patch_rust: dict[str, Any] | None
) -> list[str]:
    """未覆盖的改动行（两个语言合并一个折叠块）。"""
    entries: list[str] = []
    total_missing = 0
    omitted = 0
    for patch in (patch_python, patch_rust):
        if not patch:
            continue
        for file in patch.get("files", []):
            missing = file.get("missing") or []
            if not missing:
                continue
            total_missing += len(missing)
            if len(entries) >= MAX_MISSING_FILES:
                omitted += 1
                continue
            entries.append(f"| `{file.get('path')}` | {_fmt_lines(missing)} |")
    if not entries:
        return []
    block = [
        "",
        "<details>",
        f"<summary>未覆盖的改动行（{total_missing} 行）</summary>",
        "",
        "| 文件 | 未覆盖行 |",
        "| --- | --- |",
        *entries,
    ]
    if omitted:
        block.append(f"| … | 其余 {omitted} 个文件省略 |")
    block += ["", "</details>"]
    return block


def render(
    python_data: dict[str, Any] | None,
    rust_data: dict[str, Any] | None,
    patch_python: dict[str, Any] | None,
    patch_rust: dict[str, Any] | None,
    python_result: str | None,
    rust_result: str | None,
    run_url: str | None,
) -> str:
    patch_min = load_patch_min()
    py_totals = python_totals(python_data) if python_data else None
    rs_totals = rust_totals(rust_data) if rust_data else None

    out: list[str] = [MARKER, "", "## 📊 覆盖率", ""]
    if py_totals is None and rs_totals is None:
        out += [
            "❌ 本轮没有取到覆盖率数据 —— 两个 job 都没有产出可读的覆盖率产物。",
            "",
        ]

    # 增量（门禁）：这个 PR 的改动行 —— 状态就是 merge 门禁的判定。
    out += ["### 增量覆盖率（门禁）", ""]
    out += [
        "| 语言 | 改动行覆盖 | 覆盖 / 可执行 | 门禁 | 状态 |",
        "| --- | --- | --- | --- | --- |",
    ]
    out.append(_patch_row("Python（语句行）", patch_python, patch_min))
    out.append(_patch_row("Rust（行）", patch_rust, patch_min))
    out += _patch_missing_details(patch_python, patch_rust)

    # 总量（参考）：不再是门禁，只看趋势。
    out += ["", "### 总量（参考）", ""]
    out += ["| 范围 | 指标 | 覆盖率 |", "| --- | --- | --- |"]
    out.append(
        "| Python（`wing` + `wing_sdk`） | 语句 + 分支 | "
        f"{_fmt(py_totals[0] if py_totals else None, 1)} |"
    )
    out.append(
        f"| Rust（workspace） | 行 | {_fmt(rs_totals[0] if rs_totals else None, 2)} |"
    )
    out.append(
        f"| Rust（workspace） | 区域 | {_fmt(rs_totals[1] if rs_totals else None, 2)} |"
    )

    if python_data:
        packages = python_packages(python_data)
        if packages:
            files = len(python_data.get("files", {}))
            out += [
                "",
                "<details>",
                f"<summary>Python 分包（{files} 个文件）</summary>",
                "",
                "| 包 | 覆盖率 | 语句 | 分支 |",
                "| --- | --- | --- | --- |",
            ]
            out += [
                f"| `{package}` | {_fmt(percent, 1)} | {stmts} | {branches} |"
                for package, percent, stmts, branches in packages
            ]
            out += ["", "</details>"]

    if rust_data:
        crates = rust_crates(rust_data)
        if crates:
            files = len(rust_data["data"][0].get("files", []))
            out += [
                "",
                "<details>",
                f"<summary>Rust 分 crate（{files} 个文件）</summary>",
                "",
                "| crate | 行 | 区域 |",
                "| --- | --- | --- |",
            ]
            out += [
                f"| {crate} | {_fmt(line_pct, 2)} | {_fmt(region_pct, 2)} |"
                for crate, line_pct, region_pct in crates
            ]
            out += ["", "</details>"]

    notes: list[str] = []
    for name, data, result, patch in (
        ("Python", python_data, python_result, patch_python),
        ("Rust", rust_data, rust_result, patch_rust),
    ):
        if data is None:
            notes.append(
                f"- **{name}**：没有找到覆盖率产物（job 结果：`{result or '未知'}`）"
                " —— 见 run 日志。"
            )
        elif result is not None and result != "success":
            notes.append(
                f"- **{name}**：本轮 job 结果 `{result}` —— 若为测试失败（而非覆盖率不达标），"
                "数字可能不完整。"
            )
        if patch is None:
            notes.append(
                f"- **{name}**：增量覆盖率未判定（没有 patch 产物——门禁 step 未产出，"
                "见 job 日志）。"
            )
        elif patch.get("skipped"):
            notes.append(f"- **{name}**：增量覆盖率未判定 —— `{patch['skipped']}`。")
        elif not patch.get("ok"):
            notes.append(
                f"- **{name}**：增量覆盖率 {_fmt(float(patch['percent']), 1)} "
                f"< 门禁 {_fmt(patch_min, 1)} —— 未覆盖行见上。"
            )
        if patch:
            unmeasured = patch.get("unmeasured") or []
            if unmeasured:
                shown = "、".join(
                    f"`{f['path']}`（{f['lines']} 行）" for f in unmeasured[:6]
                )
                more = f"，另 {len(unmeasured) - 6} 个" if len(unmeasured) > 6 else ""
                notes.append(
                    f"- **{name}**：{len(unmeasured)} 个改动的非测试文件不在覆盖率统计面内"
                    f"（**不参与判定**）：{shown}{more}。"
                )
            removed_tests = patch.get("removed_tests") or []
            if removed_tests:
                total = sum(int(f["lines"]) for f in removed_tests)
                shown = "、".join(
                    f"`{f['path']}`（-{f['lines']}）" for f in removed_tests[:6]
                )
                notes.append(
                    f"- **{name}**：净删除测试 {total} 行（{shown}）——删除行不进增量判定，"
                    "请人工确认。"
                )
    if notes:
        out += ["", "---", "", "### ⚠️ 注意", "", *notes]

    footer = (
        "本地复现：`make test-python` / `make test-rust-cov` 后跑 "
        "`make patch-cov-python` / `make patch-cov-rust`"
        "（基线 `PATCH_BASE`、阈值 `PATCH_COV_MIN`，见 docs/dev/coverage.md）"
    )
    if run_url:
        footer = f"[run 日志]({run_url}) · {footer}"
    out += ["", f"<sub>{footer}</sub>", ""]
    return "\n".join(out)


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="coverage_report.py",
        description="Render the sticky coverage comment from the coverage artifacts.",
        epilog=(
            "example:\n"
            "  coverage_report.py --python-json coverage-python.json \\\n"
            "      --rust-json coverage-rust.json --patch-python patch-python.json \\\n"
            "      --patch-rust patch-rust.json --out-md comment-body.md"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--python-json", metavar="PATH", help="coverage json artifact")
    parser.add_argument("--rust-json", metavar="PATH", help="llvm-cov export artifact")
    parser.add_argument(
        "--patch-python",
        metavar="PATH",
        help="patch verdict artifact (patch_coverage.py)",
    )
    parser.add_argument(
        "--patch-rust",
        metavar="PATH",
        help="patch verdict artifact (patch_coverage.py)",
    )
    parser.add_argument(
        "--out-md",
        required=True,
        metavar="PATH",
        help="write the comment body here",
    )
    parser.add_argument("--run-url", metavar="URL", help="link to the CI run")
    parser.add_argument(
        "--python-result",
        metavar="RESULT",
        help="python-check job result (success / failure / ...), for the notes section",
    )
    parser.add_argument(
        "--rust-result",
        metavar="RESULT",
        help="rust-check job result (success / failure / ...), for the notes section",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    python_data = _read_json(args.python_json) if args.python_json else None
    rust_data = _read_json(args.rust_json) if args.rust_json else None
    patch_python = _read_json(args.patch_python) if args.patch_python else None
    patch_rust = _read_json(args.patch_rust) if args.patch_rust else None
    body = render(
        python_data,
        rust_data,
        patch_python,
        patch_rust,
        args.python_result,
        args.rust_result,
        args.run_url,
    )
    Path(args.out_md).write_text(body, encoding="utf-8")
    print(f"comment body written to {args.out_md} ({len(body)} chars)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
