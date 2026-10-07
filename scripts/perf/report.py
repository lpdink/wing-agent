#!/usr/bin/env python3
"""ab.py 输出 JSON → PR 评论 markdown（契约 §7）。

由 03 的 report job 调用：`--json <tui.json> <gateway.json>` 合并多个测量 job 的
产物后渲染成**一条**评论；`comment.py` 按首行的 `<!-- wing-perf -->` marker
查找并更新同一条。

    uv run python scripts/perf/report.py --json target/perf/ab.json --out-md /tmp/comment.md
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from common import (  # noqa: E402  (同目录模块：脚本直接跑时 sys.path 已含 HERE)
    CommandError,
    Thresholds,
    metric_unit,
    read_json,
)

#: 评论唯一标识：comment.py 据此查找 / 更新同一条评论。
MARKER = "<!-- wing-perf -->"

#: 判定 → 表格里的显示。`n/a` 覆盖两种情形：某一侧没有数据（真的不可比），
#: 以及 `thresholds.json` 的 `info_only` 信息项（只给数值、不判档）。
VERDICT_LABELS: Mapping[str, str] = {
    "regression": "🔴 回归",
    "watch": "🟠 关注",
    "improved": "🟢 改善",
    "flat": "⚪ 持平",
    "n/a": "🚫 不判定",
}

#: 结论行里的统计顺序（坏消息在前）。
VERDICT_ORDER: tuple[tuple[str, str], ...] = (
    ("regression", "项回归"),
    ("watch", "项关注"),
    ("improved", "项改善"),
    ("flat", "项持平"),
    ("n/a", "项未判定"),
)


def merge_reports(reports: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """把多份 ab 输出并成一份（顺序保持）：03 的两个测量 job 各产一份。"""
    sources = [report for report in reports if report]
    if not sources:
        raise ValueError("merge_reports() got no reports")
    first = sources[0]
    comparisons: list[Any] = []
    failures: list[Any] = []
    notes: list[str] = []
    duration = 0.0
    stub = False
    revs: set[tuple[Any, Any]] = set()
    shapes: set[tuple[Any, Any, Any]] = set()
    for report in sources:
        comparisons.extend(report.get("comparisons") or [])
        failures.extend(report.get("failures") or [])
        meta = report.get("meta") or {}
        if not isinstance(meta, Mapping):
            meta = {}
        notes.extend(str(note) for note in (meta.get("notes") or []))
        duration += float(meta.get("duration_s") or 0.0)
        stub = stub or bool(meta.get("stub"))
        revs.add((report.get("base_sha"), report.get("head_sha")))
        shapes.add(
            (
                report.get("rounds"),
                bool(report.get("quick")),
                bool(report.get("calibrate")),
            )
        )
    if len(revs) > 1:
        notes.insert(0, "合并了来自不同 sha 的报告：数字不可直接比较")
    if len(shapes) > 1:
        notes.insert(0, "合并了轮数 / 档位不一致的报告（rounds / quick / calibrate）")
    return {
        "kind": first.get("kind", "wing-perf"),
        "shape": first.get("shape", 1),
        "base_ref": first.get("base_ref"),
        "head_ref": first.get("head_ref"),
        "base_sha": first.get("base_sha"),
        "head_sha": first.get("head_sha"),
        "rounds": first.get("rounds"),
        "quick": bool(first.get("quick")),
        "calibrate": bool(first.get("calibrate")),
        "comparisons": comparisons,
        "failures": failures,
        "meta": {
            "duration_s": round(duration, 1),
            "notes": notes,
            "stub": stub,
            "sources": len(sources),
            # 合并进来的产物彼此不一致 → 表头必须放弃"轮数/档位"这类单值断言（否则误导）。
            "sha_mismatch": len(revs) > 1,
            "shape_mismatch": len(shapes) > 1,
        },
    }


def render_comment(
    report: Mapping[str, Any],
    *,
    run_url: str | None = None,
    label: str | None = None,
) -> str:
    """把（已合并的）ab 输出渲染成 PR 评论 markdown。"""
    comparisons = [
        entry
        for entry in (report.get("comparisons") or [])
        if isinstance(entry, Mapping)
    ]
    failures = [
        entry for entry in (report.get("failures") or []) if isinstance(entry, Mapping)
    ]
    meta_raw = report.get("meta")
    meta: Mapping[str, Any] = meta_raw if isinstance(meta_raw, Mapping) else {}
    base_sha = _short(report.get("base_sha"))
    head_sha = _short(report.get("head_sha"))

    lines: list[str] = [MARKER, "", "## ⚡ 性能对比（perf-ci）", ""]

    header = [f"`base` {base_sha} → `head` {head_sha}"]
    inconsistencies: list[str] = []
    if meta.get("sha_mismatch"):
        inconsistencies.append("sha 不一致")
    if meta.get("shape_mismatch"):
        inconsistencies.append("轮数 / 档位不一致")
    if inconsistencies:
        # 合并产物的单值表头会说谎：这里只报不一致，具体数值交给注意事项。
        header.append(
            "合并了多份产物：" + "、".join(inconsistencies) + "（见注意事项）"
        )
    else:
        rounds = report.get("rounds")
        if rounds:
            header.append(f"{rounds} 轮交错 A/B")
        if report.get("quick"):
            header.append("quick 档")
        if report.get("calibrate"):
            header.append("校准运行（两侧同 rev，测的是噪声地板）")
    if label:
        header.append(str(label))
    if meta.get("stub"):
        header.append("⚠️ 自测数据（非真实测量）")
    lines += [" · ".join(header), ""]
    lines += _notes_block(_notes(meta))

    lines += [_conclusion_line(comparisons, failures), ""]
    lines += _table(comparisons)
    lines += _failure_section(failures)
    lines += _footnote(report, comparisons, run_url)

    footer = []
    url = run_url or meta.get("run_url")
    if url:
        footer.append(f"[run 详情]({url})")
    footer.append(f"基准 `{base_sha}` → `{head_sha}`")
    if meta.get("stub"):
        footer.append("由 `ab.py --selftest` 生成")
    lines += ["", " · ".join(footer), ""]
    return "\n".join(lines)


def _notes(meta: Mapping[str, Any]) -> list[str]:
    """`meta.notes` → 去重后的字符串列表（顺序保持；合并告警由 merge_reports 排在最前）。"""
    raw = meta.get("notes")
    if not isinstance(raw, Sequence) or isinstance(raw, (str, bytes)):
        return []
    return list(
        dict.fromkeys(text for text in (str(item).strip() for item in raw) if text)
    )


def _notes_block(notes: Sequence[str]) -> list[str]:
    """注意事项：头部直出前 2 条（合并告警 / 未提交改动这类 caveat 必须可见），其余折叠。"""
    if not notes:
        return []
    lines = [f"> ⚠️ {note}" for note in notes[:2]]
    rest = list(notes[2:])
    if rest:
        lines.append(f"> ⚠️ 其余 {len(rest)} 条注意事项见下方折叠块。")
    lines.append("")
    if rest:
        lines += [
            f"<details><summary>注意事项（其余 {len(rest)} 条）</summary>",
            "",
            *[f"- {note}" for note in rest],
            "",
            "</details>",
            "",
        ]
    return lines


def _conclusion_line(
    comparisons: Sequence[Mapping[str, Any]], failures: Sequence[Any]
) -> str:
    counts: dict[str, int] = {}
    for entry in comparisons:
        verdict = str(entry.get("verdict", "n/a"))
        counts[verdict] = counts.get(verdict, 0) + 1
    parts = [
        f"{counts[verdict]} {text}"
        for verdict, text in VERDICT_ORDER
        if counts.get(verdict)
    ]
    summary = "；".join(parts) if parts else "无可用指标"
    if failures:
        summary += f"；⚠️ {len(failures)} 个套件失败"
    return f"**结论**：{summary}"


def _table(comparisons: Sequence[Mapping[str, Any]]) -> list[str]:
    if not comparisons:
        return ["_没有可比较的指标。_", ""]
    # 分区键是 verdict，不是"有没有 delta"：info_only 的行有 Δ 但不判档，
    # 它们要跟"某一侧无数据"一起沉底（否则它们的 note 在表里没有出口）。
    judged = [entry for entry in comparisons if str(entry.get("verdict")) != "n/a"]
    unjudged = [entry for entry in comparisons if str(entry.get("verdict")) == "n/a"]
    judged.sort(
        key=lambda entry: abs(_number(entry.get("delta_pct")) or 0.0), reverse=True
    )
    lines = ["| 指标 | base | head | Δ | 判定 |", "|---|---:|---:|---:|---|"]
    for entry in list(judged) + list(unjudged):
        lines.append(_row(entry))
    lines.append("")
    if unjudged:
        reasons = [
            f"`{entry.get('metric')}`：{entry.get('note') or '无法比较'}"
            for entry in unjudged
        ]
        lines += ["未判定：" + "；".join(reasons), ""]
    return lines


def _row(entry: Mapping[str, Any]) -> str:
    metric = str(entry.get("metric", "?"))
    unit = metric_unit(metric)
    delta = _number(entry.get("delta_pct"))
    delta_text = "—" if delta is None else f"{delta:+.1f}%"
    verdict = VERDICT_LABELS.get(str(entry.get("verdict")), str(entry.get("verdict")))
    return (
        f"| `{metric}` | {_cell(entry.get('base_median'), unit)} "
        f"| {_cell(entry.get('head_median'), unit)} | {delta_text} | {verdict} |"
    )


def _cell(value: Any, unit: str) -> str:
    number = _number(value)
    if number is None:
        return "—"  # 缺失侧不挂单位，避免 `— ms` 这类误导
    text = fmt_value(number)
    return f"{text} {unit}".strip() if unit else text


def _failure_section(failures: Sequence[Mapping[str, Any]]) -> list[str]:
    if not failures:
        return []
    lines = ["### ⚠️ 套件失败（基础设施故障，不代表性能结论）", ""]
    details: list[str] = []
    for entry in failures:
        label = f"{entry.get('suite')}/{entry.get('side') or '-'}"
        if entry.get("round") is None:
            where = f"{label}（prepare）"
        else:
            where = f"{label} r{entry.get('round')}"
        error = str(entry.get("error") or "未记录原因")
        first = error.strip().splitlines()[0] if error.strip() else "未记录原因"
        lines.append(f"- `{where}`：{first}")
        if len(error.strip().splitlines()) > 1 or entry.get("log_tail"):
            details.append(
                f"**{where}**\n\n```\n{str(entry.get('log_tail') or error).strip()}\n```"
            )
    lines.append("")
    if details:
        lines += [
            "<details><summary>失败日志尾部</summary>",
            "",
            "\n\n".join(details),
            "",
            "</details>",
            "",
        ]
    return lines


def _footnote(
    report: Mapping[str, Any],
    comparisons: Sequence[Mapping[str, Any]],
    run_url: str | None,
) -> list[str]:
    noise, regress = _thresholds_used(comparisons)
    meta_raw = report.get("meta")
    meta: Mapping[str, Any] = meta_raw if isinstance(meta_raw, Mapping) else {}
    if meta.get("shape_mismatch"):
        # 合并了档位/轮数不一致的产物：不能报"本次 N 轮"这种单值口径。
        rounds_text = "多份报告（轮数 / 档位不一致，见注意事项）"
    else:
        rounds_text = f"本次 {report.get('rounds') or '?'} 轮"
    bullets = [
        f"- **交错 A/B**：同一 runner 内 `base`/`head` 逐轮交错（base r1 → head r1 → …），"
        f"每指标先取每轮中位数、再取轮间中位数；{rounds_text}。",
        f"- **判定**：|Δ| 小于该指标的噪声带记「持平」，劣化达到回归线记「回归」，"
        f"两者之间记「关注」，方向变好记「改善」（默认 lower-better）。本次最常用的"
        f"带宽：噪声 {noise}% / 回归 {regress}%；p99 型等指标族有更宽的覆盖，"
        "完整名单见 `scripts/perf/thresholds.json`。",
        "- **`🚫 不判定`**：某一侧没有数据（真的不可比），或该指标是信息项"
        "（如 `tui.display.coverage_ratio` 是测量质量、不是显示性能，只报数值）。",
        "- **口径**：排除冷启动；TUI / Gateway 用假 Provider（零 LLM 时延）；两侧由同一份 "
        "harness 代码驱动（`scripts/perf/`），只允许代码来源不同。",
        "- **注意**：runner 噪声、样本量与并行负载都会影响数字；本评论只作参考，不是合并门禁。"
        "修正方向见 `scripts/perf/thresholds.json` 与 `--calibrate` 校准运行。",
    ]
    if report.get("calibrate"):
        bullets.append(
            "- **校准运行**：两侧同 rev，理论上 Δ 应集中在 0 附近；偏大说明噪声地板偏高。"
        )
    if run_url:
        bullets.append(f"- **run**：{run_url}")
    return [
        "<details><summary>方法与口径</summary>",
        "",
        *bullets,
        "",
        "</details>",
        "",
    ]


def _thresholds_used(comparisons: Sequence[Mapping[str, Any]]) -> tuple[float, float]:
    """本次跑**最常用**的 (noise_pct, regress_pct)。

    每个指标族可以有自己的带宽（`thresholds.json.overrides`，如 p99 型更宽），任何单一
    数字都只是近似——取出现次数最多的一对，避免"取最大值"把某个宽带宽吹成全局口径。
    """
    counts: dict[tuple[float, float], int] = {}
    for entry in comparisons:
        noise = _number(entry.get("noise_pct"))
        regress = _number(entry.get("regress_pct"))
        if noise is None or regress is None:
            continue
        key = (noise, regress)
        counts[key] = counts.get(key, 0) + 1
    if not counts:
        fallback = _load_thresholds()
        return (fallback.noise_pct, fallback.regress_pct)
    # 次数降序，平手时取更小的带宽（更保守的表述）。
    return max(counts.items(), key=lambda item: (item[1], -item[0][0], -item[0][1]))[0]


def _load_thresholds() -> Thresholds:
    path = HERE / "thresholds.json"
    if path.is_file():
        return Thresholds.load(path)
    return Thresholds()


def _number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def fmt_value(value: float | None) -> str:
    """表格里的数值显示：按量级选精度，小值不塌成 "0"（ratio 类指标会读成零）。"""
    if value is None:
        return "—"
    number = float(value)
    if number == 0:
        return "0"
    magnitude = abs(number)
    if magnitude >= 1000:
        return f"{number:,.0f}"
    if magnitude >= 1:
        return f"{number:,.3f}".rstrip("0").rstrip(".")
    if magnitude >= 0.01:
        return f"{number:.4f}".rstrip("0").rstrip(".")
    if magnitude >= 0.0001:
        return f"{number:.6f}".rstrip("0").rstrip(".")
    return f"{number:.3g}"  # 1e-4 以下用科学计数：0.0000123 渲染成 "0" 是错的


def _short(sha: Any) -> str:
    text = str(sha or "?")
    return text[:7] if len(text) > 7 else text


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="render the wing-perf PR comment (markdown) from ab.py JSON files"
    )
    parser.add_argument(
        "--json",
        nargs="+",
        required=True,
        help="one or more ab.py output JSON files (merged in order)",
    )
    parser.add_argument(
        "--out-md", help="write the markdown here (default: print to stdout)"
    )
    parser.add_argument(
        "--run-url", help="CI run / artifact URL rendered in the footer"
    )
    parser.add_argument("--label", help="extra header label, e.g. 'PR #12 · run 345'")
    args = parser.parse_args(argv)

    try:
        reports = [read_json(Path(path)) for path in args.json]
        merged = merge_reports(reports)
        markdown = render_comment(merged, run_url=args.run_url, label=args.label)
    except (CommandError, ValueError) as exc:
        # CI 里只看到 traceback 没法排查：给出可操作的一行错误 + 明确的退出码。
        print(f"[report] error: {exc}", file=sys.stderr)
        return 2
    if args.out_md:
        out = Path(args.out_md)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(markdown, encoding="utf-8")
        print(f"[report] wrote {out}")
    else:
        print(markdown)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
