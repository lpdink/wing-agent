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
高于 `--max-load`（默认 4.0：12 核上约 1/3）时直接拒绝跑，因为此时
任何差异都可能是噪声。确实要在负载下比（比如只想看趋势）用 `--allow-load`。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from collections.abc import Callable
from pathlib import Path
from typing import Any

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
        raw = None
    finally:
        report_path.unlink(missing_ok=True)

    if raw is None:
        # 老 checkout 的 `collect_output.sh` 还不认识 WING_TEST_REPORT（基线侧常常如此）。
        # 退回解析终端里那几行 `└ <组> 完成，用时 <n>s`：精度只有整秒，但墙钟由这里
        # 自己测（毫秒），逐组那一列够用来对照。拿不到任何组就说明脚本早退了。
        groups_s, codes_s = parse_human_output(proc.stdout)
        if not groups_s:
            print(proc.stdout, file=sys.stderr)
            raise SystemExit(
                f"collect_output.sh 没有写出报告、也没有可解析的组耗时（{tree}）"
            )
        return Round(
            wall_ms=wall_ms,
            groups=groups_s,
            codes=codes_s,
            stats={},
            load=load_average(),
        )

    groups, codes, stats = parse_report(raw)
    return Round(
        wall_ms=wall_ms,
        groups=groups,
        codes=codes,
        stats=stats,
        load=load_average(),
    )


def parse_report(
    raw: dict[str, Any],
) -> tuple[dict[str, int], dict[str, int], dict[str, str]]:
    """`collect_output.sh` 的报告 → (逐组毫秒, 逐组退出码, 逐组统计行)。

    形状就是 `{"groups": [{label, code, ms, stat}]}`——写方与读方之间只有这一处契约，
    `--selftest` 用固定样本钉住它（`scripts/test_rust_parallel.py --selftest` 的同一个惯例）。
    """
    groups: dict[str, int] = {}
    codes: dict[str, int] = {}
    stats: dict[str, str] = {}
    for entry in raw["groups"]:
        groups[entry["label"]] = int(entry["ms"])
        codes[entry["label"]] = int(entry["code"])
        stats[entry["label"]] = entry["stat"]
    return groups, codes, stats


#: `└ python 完成，用时 16s` / `└ probe  失败（exit=1），用时 33s`——逐组耗时行。
_HUMAN_PROGRESS = re.compile(
    r"└\s+(?P<label>\S+)\s+(?P<failed>失败（exit=(?P<code>\d+)）)?.*?用时\s+(?P<secs>\d+)s"
)


def parse_human_output(output: str) -> tuple[dict[str, int], dict[str, int]]:
    """从终端输出里取逐组（毫秒近似的整秒 | 退出码）。"""
    groups: dict[str, int] = {}
    codes: dict[str, int] = {}
    for match in _HUMAN_PROGRESS.finditer(output):
        label = match.group("label")
        groups[label] = int(match.group("secs")) * 1000
        codes[label] = int(match.group("code") or 0)
    return groups, codes


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


def _passed_counts(rounds: list[Round], label: str) -> int | None:
    """这一组跑过多少条用例（从统计行里取 `N passed`；取不到就 None）。"""
    for rnd in reversed(rounds):
        match = re.search(r"(\d+) passed", rnd.stats.get(label, ""))
        if match:
            return int(match.group(1))
    return None


def _print_counts(labels: list[str], rounds: dict[str, list[Round]]) -> None:
    """用例数并排印出来：只快不少，可能只是少跑了。

    基线侧常常没有 `WING_TEST_REPORT` 支持（老 checkout），那时统计行是空的——
    打 `?` 而不是沉默，读的人才不会把"没数"当成"数量一致"。
    """
    print("\n── 用例数（各侧最后一轮的统计行）──")
    for name in labels:
        counts = [
            f"{label}: {_passed_counts(rounds[name], label) or '?'}"
            for label in sorted(rounds[name][-1].stats) or ["?"]
        ]
        print(f"  {name}: {'、'.join(counts) if counts else '（无统计行）'}")


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


def selftest() -> int:
    """离线自检解析与聚合（不跑任何测试）。

    这一层唯一会静默出错的地方是"读错数"：报告形状变了、终端文案变了、中位数算法偏了，
    出来的都是看着正常但错了一个量级的数字——比崩溃危险得多，所以用固定样本钉住。
    """
    failures = 0

    def check(name: str, got: object, want: object) -> None:
        nonlocal failures
        ok = got == want
        failures += 0 if ok else 1
        print(
            f"[selftest] {'ok  ' if ok else 'FAIL'} {name} = {got!r}{'' if ok else f' (want {want!r})'}"
        )

    raw = {
        "groups": [
            {"label": "python", "code": 0, "ms": 4800, "stat": "4098 passed"},
            {"label": "probe", "code": 1, "ms": 17000, "stat": "1 failed"},
        ]
    }
    groups, codes, stats = parse_report(raw)
    check("report groups", groups, {"python": 4800, "probe": 17000})
    check("report codes", codes, {"python": 0, "probe": 1})
    check("report stats", stats["probe"], "1 failed")

    human = (
        "▶ python make test-python（日志：/tmp/x.log）\n"
        "  └ python 完成，用时 5s\n"
        "  └ probe  失败（exit=1），用时 17s\n"
    )
    check(
        "human output",
        parse_human_output(human),
        ({"python": 5000, "probe": 17000}, {"python": 0, "probe": 1}),
    )
    check("human output (走神文本)", parse_human_output("no progress lines"), ({}, {}))

    rounds = [
        Round(wall_ms=1000, groups={"a": 100}, codes={"a": 0}),
        Round(wall_ms=3000, groups={"a": 300}, codes={"a": 0}),
        Round(wall_ms=2000, groups={"a": 200}, codes={"a": 0}),
    ]
    check("median wall", describe(rounds), (2000, {"a": 200}))
    check("seconds", seconds(2500), "2.5s")

    failures += _knob_selftest(check)

    print(f"[selftest] {'ok' if not failures else f'{failures} failure(s)'}")
    return 0 if not failures else 1


def _knob_selftest(check: Callable[[str, object, object], None]) -> int:
    """四组 × {独占, 预算} 是否真的变成了**命令行实参**。

    这条链断过一次（`pnpm run test -- <args>` 的实参被 pnpm 吞掉，共享档与逃生舱双双
    静默失效，而测试全绿）——两边各自"能解析"不等于"传到了"。所以这里直接问 make：
    它展开出来的命令行长什么样。
    """
    root = Path(__file__).resolve().parents[1]
    if not (root / "Makefile").is_file():  # pragma: no cover - 单独拷脚本出去时
        print("[selftest] skip knobs（找不到 Makefile）")
        return 0

    def dry_run(target: str, **env: str) -> str:
        merged = {**os.environ, **env}
        for name in (
            "WING_TEST_SERIAL",
            "WING_TEST_BUDGET_PYTEST",
            "WING_TEST_BUDGET_TS",
        ):
            if name not in env:
                merged.pop(name, None)
        proc = subprocess.run(
            ["make", "-n", target],
            cwd=str(root),
            env=merged,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        return proc.stdout

    py_solo = dry_run("test-python")
    check("knobs python solo has xdist", "--dist loadfile -n " in py_solo, True)
    py_budget = dry_run("test-python", WING_TEST_BUDGET_PYTEST="4")
    check("knobs python budget reaches argv", "--dist loadfile -n 4" in py_budget, True)
    py_serial = dry_run("test-python", WING_TEST_SERIAL="1")
    check("knobs python serial drops xdist", "-n " in py_serial, False)

    ts_budget = dry_run("test-ts", WING_TEST_BUDGET_TS="4")
    check(
        "knobs ts budget reaches argv",
        "pnpm run test --maxWorkers=4" in ts_budget,
        True,
    )
    # pnpm 会把 `--` 和它后面的实参一起吞掉（vitest 一个都收不到，且不报错）。
    check(
        "knobs ts never uses the swallowed form", "-- --maxWorkers" in ts_budget, False
    )
    return 0


def main(argv: list[str]) -> int:
    if argv == ["--selftest"]:
        return selftest()
    parser = argparse.ArgumentParser(
        prog="bench_tests.py",
        description="make test / make check 的交错 A/B 基准（`--selftest` 只自检解析，不跑测试）",
    )
    parser.add_argument(
        "--tree",
        action="append",
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
    parser.add_argument(
        "--allow-red",
        action="store_true",
        help="某一轮没有全绿时继续（只比时间，失败在结论里逐轮列出）",
    )
    parser.add_argument(
        "--pause",
        type=float,
        default=5.0,
        help="两次运行之间空转几秒，让 CPU / 文件缓存回落（默认 5）",
    )
    args = parser.parse_args(argv)

    if not args.tree:
        parser.error("至少要给一个 --tree（`--selftest` 不用跑测试）")
    trees = [Path(path).expanduser().resolve() for path in args.tree]
    labels = args.label or [tree.name for tree in trees]
    if len(labels) != len(trees):
        parser.error("--label 的个数必须与 --tree 相同")
    if len(set(labels)) != len(labels):
        # 同名会让两棵树的轮次落进同一个桶，静默输出"1.00×"。
        parser.error(f"label 必须唯一：{labels}")
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
            if not warm.ok and not args.allow_red:
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
            if not result.ok and not args.allow_red:
                print(f"❌ {name} 的 {args.mode} 没有全绿，基准中止", file=sys.stderr)
                return 1
            if args.pause > 0:
                time.sleep(args.pause)

    summary: dict[str, tuple[int, dict[str, int]]] = {}
    for name in labels:
        summary[name] = describe(rounds[name])
        print_table(f"── {name} ──", rounds[name], *summary[name])

    red = [
        (name, index + 1, sorted(label for label, code in r.codes.items() if code))
        for name in labels
        for index, r in enumerate(rounds[name])
        if not r.ok
    ]
    if red:
        print("\n注意：以下轮次没有全绿（墙钟仍然计入中位数）：")
        for name, round_no, labels_red in red:
            print(f"  {name} 第 {round_no} 轮：{', '.join(labels_red)}")

    _print_counts(labels, rounds)

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
