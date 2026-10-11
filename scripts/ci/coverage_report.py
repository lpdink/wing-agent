#!/usr/bin/env python3
"""Python / Rust 覆盖率数据 → PR 评论 markdown。

ci.yml 的 python-check / rust-check 两个 job 各产出一份机器可读覆盖率产物
（coverage json / llvm-cov export），本脚本合并渲染成**一条** sticky 评论；
`sticky_comment.py` 按首行的 `<!-- wing-coverage -->` marker 查找并原地更新。

阈值不在这里声明 —— 两边都读各自门禁的唯一事实来源：Python 取 pyproject.toml 的
`[tool.coverage.report]`（fail_under / precision），Rust 取 Makefile 的 `RUST_COV_*`
变量。任一数据源缺失（job 失败 / 产物没产出）时，对应行显示 `—` 并在「注意」里说明，
评论照发 —— 覆盖率没到或测试挂了，恰恰是最需要看到这条评论的时候。

    python3 scripts/ci/coverage_report.py \
        --python-json coverage-python.json --rust-json coverage-rust.json \
        --out-md comment-body.md --run-url https://github.com/.../actions/runs/123
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]

#: 评论唯一标识：sticky_comment.py 据此查找 / 更新同一条评论。
MARKER = "<!-- wing-coverage -->"


def _read_json(path: str) -> dict[str, Any] | None:
    """读一份覆盖率产物；读不出（缺失 / 坏 JSON）返回 None，由调用方走「数据缺失」分支。"""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"warning: cannot read {path}: {error}", file=sys.stderr)
        return None
    return data if isinstance(data, dict) else None


def load_python_gate() -> tuple[float, int] | None:
    """pyproject.toml 的 [tool.coverage.report] → (fail_under, precision)。"""
    try:
        config = tomllib.loads(
            (REPO_ROOT / "pyproject.toml").read_text(encoding="utf-8")
        )
    except OSError:
        return None
    report = config.get("tool", {}).get("coverage", {}).get("report", {})
    fail_under, precision = report.get("fail_under"), report.get("precision", 0)
    if not isinstance(fail_under, int | float) or not isinstance(precision, int):
        return None
    return float(fail_under), precision


def load_rust_gate() -> tuple[float, float] | None:
    """Makefile 的 RUST_COV_LINES_MIN / RUST_COV_REGIONS_MIN → (行, 区域)。"""
    try:
        text = (REPO_ROOT / "Makefile").read_text(encoding="utf-8")
    except OSError:
        return None
    lines = re.search(r"^RUST_COV_LINES_MIN\s*\??=\s*([0-9.]+)", text, re.M)
    regions = re.search(r"^RUST_COV_REGIONS_MIN\s*\??=\s*([0-9.]+)", text, re.M)
    if not lines or not regions:
        return None
    return float(lines.group(1)), float(regions.group(1))


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


def _status(percent: float | None, threshold: float | None, digits: int | None) -> str:
    """✅ / ❌ / —：Python 侧按 precision 舍入后再比（与 coverage 的 fail_under 同口径），
    Rust 侧用原始值（与 cargo-llvm-cov 的比较同口径）。"""
    if percent is None or threshold is None:
        return "—"
    value = round(percent, digits) if digits is not None else percent
    return "✅" if value >= threshold else "❌"


def _fmt(percent: float | None, digits: int) -> str:
    return f"{percent:.{digits}f}%" if percent is not None else "—"


def render(
    python_data: dict[str, Any] | None,
    rust_data: dict[str, Any] | None,
    python_result: str | None,
    rust_result: str | None,
    run_url: str | None,
) -> str:
    python_gate = load_python_gate()
    rust_gate = load_rust_gate()
    py_totals = python_totals(python_data) if python_data else None
    rs_totals = rust_totals(rust_data) if rust_data else None

    py_percent = py_totals[0] if py_totals else None
    py_threshold = python_gate[0] if python_gate else None
    py_digits = python_gate[1] if python_gate else 1

    out: list[str] = [MARKER, "", "## 📊 覆盖率", ""]
    if py_totals is None and rs_totals is None:
        out += [
            "❌ 本轮没有取到覆盖率数据 —— 两个 job 都没有产出可读的覆盖率产物。",
            "",
        ]

    out += ["| 范围 | 指标 | 覆盖率 | 阈值 | 状态 |", "| --- | --- | --- | --- | --- |"]
    out.append(
        "| Python（`wing` + `wing_sdk`） | 语句 + 分支"
        f" | {_fmt(py_percent, py_digits)} | {_fmt(py_threshold, 1)}"
        f" | {_status(py_percent, py_threshold, py_digits)} |"
    )
    for label, percent, threshold in (
        (
            "行",
            rs_totals[0] if rs_totals else None,
            rust_gate[0] if rust_gate else None,
        ),
        (
            "区域",
            rs_totals[1] if rs_totals else None,
            rust_gate[1] if rust_gate else None,
        ),
    ):
        out.append(
            f"| Rust（workspace） | {label}"
            f" | {_fmt(percent, 2)} | {_fmt(threshold, 1)}"
            f" | {_status(percent, threshold, None)} |"
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
    for name, data, result in (
        ("Python", python_data, python_result),
        ("Rust", rust_data, rust_result),
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
    if notes:
        out += ["", "---", "", "### ⚠️ 注意", "", *notes]

    footer = "本地复现：`make test-python` / `make test-rust`（口径与阈值同 CI）"
    if run_url:
        footer = f"[run 日志]({run_url}) · {footer}"
    out += ["", f"<sub>{footer}</sub>", ""]
    return "\n".join(out)


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="coverage_report.py",
        description="Render the sticky coverage comment from the two coverage artifacts.",
        epilog=(
            "example:\n"
            "  coverage_report.py --python-json coverage-python.json \\\n"
            "      --rust-json coverage-rust.json --out-md comment-body.md"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--python-json", metavar="PATH", help="coverage json artifact")
    parser.add_argument("--rust-json", metavar="PATH", help="llvm-cov export artifact")
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
    body = render(
        python_data, rust_data, args.python_result, args.rust_result, args.run_url
    )
    Path(args.out_md).write_text(body, encoding="utf-8")
    print(f"comment body written to {args.out_md} ({len(body)} chars)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
