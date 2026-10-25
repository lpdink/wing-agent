#!/usr/bin/env python3
"""``make test`` / ``make check`` 的 A/B 基准 —— 提速多少，用交错实测说话。

动机：单次 `make test` 的墙钟在满载机器上能翻倍（同一份代码 33s 与 67s 都见过）。
"改前 33s、改后 20s"这种比法把负载漂移算成了收益，站不住。这里做两件事：

1. **交错**：每一轮在 A、B 两个 checkout 上各跑一次（相邻发生，负载条件相同），
   轮次之间交换先后顺序，抵消单向漂移；报告取各轮中位数。
2. **拆到组**：`scripts/collect_output.sh` 在 `WING_TEST_REPORT=<path>` 下发一份
   JSON（组名 / 毫秒 / 退出码 / 统计行），据此给出逐组对照——否则"总时长变短"
   可能只是某一组变快掩盖了另一组变慢。

用法：

    uv run python scripts/bench_tests.py \\
        --tree /path/to/baseline --tree /path/to/candidate --repeat 3

每个 tree 是**一个独立的 checkout**（worktree 即可），各自预热一次后开始计分。
预热不可省：Rust 的 `cargo test --no-run` 与 Python 的字节码缓存都会把首轮拉长，
而那是构建成本，不是测试成本——它会淹没被测的差异（`--no-warm` 只在你想看
"冷启动"时用）。

前提：机器上不该有别的活。脚本每轮打印 1/5/15 分钟负载均值供贴进 PR；
高于 `--max-load`（默认 4.0，即"核数的一半"量级）时直接拒绝跑，因为此时
任何差异都可能是噪声。确实要在负载下比（比如只想看趋势）用 `--allow-load`。
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

#: `scripts/collect_output.sh` 读这个变量决定是否落机器读的报告。
REPORT_ENV = "WING_TEST_REPORT"

#: 负载门禁的默认阈值（load average 1 分钟）。12 核机器上半数核被占用时，
#: 测试墙钟已经开始明显受影响；超过它比对的是噪声而不是代码。
DEFAULT_MAX_LOAD = 4.0


@dataclass
class Round:
    """一次运行：墙钟（毫秒）+ 逐组（毫秒 + 退出码 + 统计行）。"""

    wall_ms: int
    groups: dict[str, int] = field(default_factory=dict)
    codes: dict[str, int] = field(default_factory=dict)
    stats: dict[str, str] = field(default_factory=dict)
    load: tuple[float, float, float] = (0.0, 0.0, 0.0)

    @property
    def ok(self) -> bool:
        return all(code == 0 for code in self.codes.values())


KIND_CHOICES = ("test", "check")


def load_average() -> tuple[float, float, float]:
    try:
        one, five, fifteen = os.getloadavg()
    except OSError:  # pragma: no cover — 平台不支持时不该让基准失败
        return (0.0, 0.0, 0.0)
    return (one, five, fifteen)


def run_once(tree: Path, mode: str) -> Round:
    """在一个 checkout 上跑一次 `collect_output.sh`，返回墙钟与逐组耗时。"""
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as handle:
        report_path = Path(handle.name)
    report_path.unlink(missing_ok=True)  # 让"文件不存在"能区分"脚本没跑到"
    env = dict(os.environ)
    env[REPORT_ENV] = str(report_path)
    started = time.perf_counter()
    proc = subprocess.run(
        ["bash", "scripts/collect_output.sh", mode],
        cwd=str(tree),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    wall_ms = int((time.perf_counter() - started) * 1000)
    try:
        raw = json.loads(report_path.read_text())
    except (OSError, json.JSONDecodeError):
        # 报告缺失只可能是脚本本身早退（缺工具链、参数错）：把现场交出去，
        # 不要用"墙钟跑完了"假装成功。
        print(proc.stdout, file=sys.stderr)
        raise SystemExit(f"collect_output.sh 没有写出报告（{tree}），见上方输出")
    finally:
        report_path.unlink(missing_ok=True)

    groups: dict[str, int] = {}
    codes: dict[str, int] = {}
    stats: dict[str, str] = {}
    for entry in raw["groups"]:
        groups[entry["label"]] = entry["ms"]
        codes[entry["label"]] = entry["code"]
        stats[entry["label"]] = entry["stat"]
    return Round(
        wall_ms=wall_ms,
        groups=groups,
        codes=codes,
        stats=stats,
        load=load_average(),
    )


def median(values: list[int]) -> int:
    return int(statistics.median(values))


def describe(rounds: list[Round]) -> tuple[int, dict[str, int]]:
    """多轮 → (墙钟中位数, 逐组中位数)。缺失的组（没跑）不出现在结果里。"""
    wall = median([r.wall_ms for r in rounds])
    labels = sorted({label for r in rounds for label in r.groups})
    return wall, {
        label: median([r.groups[label] for r in rounds if label in r.groups])
        for label in labels
    }


def seconds(ms: int) -> str:
    return f"{ms / 1000:.1f}s"


def print_table(
    title: str, rounds: list[Round], wall: int, groups: dict[str, int]
) -> None:
    print(f"\n{title}")
    print(
        f"  墙钟中位数 {seconds(wall)}（{len(rounds)} 轮：{'、'.join(seconds(r.wall_ms) for r in rounds)}）"
    )
    for label in sorted(groups, key=lambda name: -groups[name]):
        print(
            f"    {label:<7} {seconds(groups[label]):>7}  {rounds[-1].stats.get(label, '')}"
        )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="bench_tests.py",
        description="make test / make check 的交错 A/B 基准",
    )
    parser.add_argument(
        "--tree",
        action="append",
        required=True,
        metavar="PATH",
        help="一个 checkout 的路径（可重复：第一个是基线，第二个是对照）",
    )
    parser.add_argument("--repeat", type=int, default=3, help="交错轮数（默认 3）")
    parser.add_argument(
        "--mode", choices=KIND_CHOICES, default="test", help="跑 test 还是 check"
    )
    parser.add_argument(
        "--label", action="append", metavar="NAME", help="与 --tree 一一对应的显示名"
    )
    parser.add_argument(
        "--no-warm", action="store_true", help="跳过预热（想看冷启动时才用）"
    )
    parser.add_argument(
        "--max-load",
        type=float,
        default=DEFAULT_MAX_LOAD,
        help=f"1 分钟负载均值上限（默认 {DEFAULT_MAX_LOAD}）",
    )
    parser.add_argument("--allow-load", action="store_true", help="忽略负载门禁")
    args = parser.parse_args(argv)

    trees = [Path(path).expanduser().resolve() for path in args.tree]
    labels = args.label or [tree.name for tree in trees]
    if len(labels) != len(trees):
        parser.error("--label 的个数必须与 --tree 相同")
    for tree in trees:
        if not (tree / "scripts" / "collect_output.sh").is_file():
            parser.error(f"{tree} 看起来不是 wing-agent 的 checkout")

    load = load_average()
    print(f"负载（1/5/15 分钟）：{load[0]:.2f} / {load[1]:.2f} / {load[2]:.2f}")
    if load[0] > args.max_load and not args.allow_load:
        print(
            f"❌ 1 分钟负载 {load[0]:.2f} > {args.max_load}：此时比对的是噪声。\n"
            "   等本机没有别的任务在跑（`wing ps` 确认没有 working 的会话），或明确用 --allow-load 记录趋势。",
            file=sys.stderr,
        )
        return 2

    if not args.no_warm:
        for name, tree in zip(labels, trees, strict=True):
            print(f"🔥 预热 {name}（{tree}）…", flush=True)
            warm = run_once(tree, args.mode)
            print(f"   预热完成 {seconds(warm.wall_ms)}（不计入结果）", flush=True)
            if not warm.ok:
                print(
                    f"❌ 预热就失败了：{name} 的 {args.mode} 没有全绿", file=sys.stderr
                )
                return 1

    rounds: dict[str, list[Round]] = {name: [] for name in labels}
    for index in range(args.repeat):
        # 顺序逐轮翻转：A→B、B→A、A→B…，单向漂移（机器越来越热 / 越来越凉）被摊平。
        order = list(range(len(trees)))
        if index % 2 == 1:
            order.reverse()
        for slot in order:
            name, tree = labels[slot], trees[slot]
            print(f"▶ 第 {index + 1}/{args.repeat} 轮 {name} …", flush=True)
            result = run_once(tree, args.mode)
            rounds[name].append(result)
            status = "✅" if result.ok else "❌"
            print(
                f"  {status} {seconds(result.wall_ms)}  load={result.load[0]:.2f}",
                flush=True,
            )
            if not result.ok:
                print(f"❌ {name} 的 {args.mode} 没有全绿，基准中止", file=sys.stderr)
                return 1

    summary: dict[str, tuple[int, dict[str, int]]] = {}
    for name in labels:
        summary[name] = describe(rounds[name])
        print_table(f"── {name} ──", rounds[name], *summary[name])

    if len(trees) == 2:
        base_wall, base_groups = summary[labels[0]]
        cand_wall, cand_groups = summary[labels[1]]
        print(f"\n── 提速（{labels[1]} 相对 {labels[0]}）──")
        print(
            f"  墙钟  {seconds(base_wall)} → {seconds(cand_wall)}  ({base_wall / cand_wall:.2f}×)"
        )
        for label in sorted(
            set(base_groups) & set(cand_groups), key=lambda n: -base_groups[n]
        ):
            before, after = base_groups[label], cand_groups[label]
            if after == 0:
                continue
            print(
                f"  {label:<7} {seconds(before):>7} → {seconds(after):>7}  ({before / after:.2f}×)"
            )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
