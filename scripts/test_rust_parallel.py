#!/usr/bin/env python3
"""``cargo test --workspace`` 的跨二进制并行版本 —— ``make test-rust`` 的实现。

cargo 逐个串行执行测试二进制（只有**单个二进制内部**才多线程），55 个套件的耗时直接
相加，而其中一半以上的工作量压在 ``wing`` 的 lib 单测一个套件里。这里复用
``cargo test --no-run`` 的构建产物，按"这个池子认为自己独占几个核"同时铺开多个二进制，
把长尾摊平。

**输出契约**（``scripts/collect_output.sh`` 依赖它）：每条 ``test result: …`` 在
日志里**恰好出现一次**（通过的套件在完成时打印，失败的套件只出现在完整输出里），
统计行按套件求和。构建失败时 rustc 诊断按 cargo 的渲染形式打印，退出码用 cargo 的。

**并行度**：一个旋钮 ``--cores``（默认 = 核数；``WING_TEST_SERIAL=1`` 与另外两组
同义 → 1；``--serial`` 同义），分两条规则花掉：

1. 每个二进制的 libtest 线程数 = 它在全局用例数里的**份额** × 核数（下界
   ``_THIN_FLOOR``、上界核数，见 ``_threads_for``）；
2. 同时在跑的二进制数 = 核数。

于是**同时在飞的线程数不是 ``--cores``**：``--cores N`` 的线程峰值 ≈ ``N × _THIN_FLOOR``
再加最重那个套件的余额（12 核上实测 32 条）。这是有意的——集成套件的线程大半时间在等
子进程 / 端口 / pty，多给它们线程不占 CPU，而 CPU 密集的套件按份额拿线程。

用例数是"这个套件有多重"最便宜的量级代理：55 个二进制并发 ``--list`` 一共 0.14s。
它的盲区是"条数少但每条重"的套件（会退到下界）——把它们拆成更多用例只在池子给得出
更多线程时才有用，否则它们就是尾部长。发车顺序取用例数降序（最长先跑）；doctests 是独立的 cargo
调用，排在第一个提交，不与池子串行。

``--selftest`` 离线自检本脚本的三条规则（不跑测试），``scripts/ci/patch_coverage.py
--selftest`` 是同一个惯例。

单套件 / 名称过滤请直接用 ``cargo test``：这里只服务"全量跑一遍"。
"""

from __future__ import annotations

import contextlib
import json
import os
import subprocess
import sys
import threading
import time
from concurrent.futures import Future, ThreadPoolExecutor
from collections.abc import Iterator
from dataclasses import dataclass, replace

#: 与 ``scripts/collect_output.sh`` 的逃生舱同名：排查并发干扰时四组一起退化成串行。
SERIAL_ENV = "WING_TEST_SERIAL"

#: 四组并行时这一组分到的核数（由 `scripts/collect_output.sh` 导出）。与 `SERIAL_ENV`
#: 正交——串行逃生舱优先级更高。
BUDGET_ENV = "WING_TEST_BUDGET_RUST"

#: 探测用例数时并发起多少个 ``--list``：一次进程启动 + 枚举用例名，16 路足够把整段
#: 压到 0.1s 量级（再高也快不了，受限于 55 次进程启动的总量）。
_LIST_WORKERS = 16

#: 每个套件至少拿到的 libtest 线程数（见 ``_threads_for``）。
_THIN_FLOOR = 2

#: ``--list`` 的输出格式：「每条用例一行，已注册的以 ``: test`` 结尾，未注册的以
#: ``: bench`` 结尾」——只数前者（bench 不参与 ``cargo test`` 的常规运行）。
_LIST_FORMAT = "terse"


@dataclass(frozen=True)
class Suite:
    """一个测试二进制（libtest 的构建产物）。

    ``label`` 是日志里的显示名：lib 单测与同名 bin 单测（如 ``wing``）会撞名，
    撞名时补上 target kind，免得失败清单看起来像重复跑了同一套件。

    ``tests`` 是用例数（``--list`` 数出来的）：它同时决定线程数与发车顺序。
    """

    label: str
    executable: str
    tests: int


@dataclass(frozen=True)
class Result:
    suite: Suite
    code: int
    elapsed: float
    output: str
    threads: int | None = None


def _cpu_count() -> int:
    return os.cpu_count() or 4


def _serial() -> bool:
    """``WING_TEST_SERIAL=1``（非空且非 0）——与 python / probe 两组的判定一致。"""
    value = os.environ.get(SERIAL_ENV, "")
    return bool(value) and value != "0"


def _default_cores() -> int:
    """这个池子认为自己独占几个核（串行逃生舱下为 1）。``--cores`` 覆盖它。

    ``WING_TEST_BUDGET_RUST`` 是 ``scripts/collect_output.sh`` 在四组并行时发的份额
    （数值，那一处是全仓唯一算它的地方：同一条公式写三遍必然分叉）。没有它就按独占算——
    单独跑 ``make test-rust`` 的人应该拿满机器。
    """
    if _serial():
        return 1
    budget = os.environ.get(BUDGET_ENV, "").strip()
    if budget.isdigit() and int(budget) > 0:
        return int(budget)
    return _cpu_count()


def _threads_for(tests: int, total_tests: int, cores: int) -> int:
    """这个套件的 libtest 线程数：它在全局用例数里的**份额** × 核数。

    下界 ``_THIN_FLOOR``（核数比它还小时退化为核数），上界核数。在当前用例分布下这个
    公式实际只产生两档：``wing`` 的 lib 单测（占八成用例、近一半的 CPU）拿到接近满额，
    其余 54 个套件全部落在下界——"重活按份额拿线程、轻活一两条线程"就是它想表达的东西，
    不需要一张写死的名单。

    下界为什么是 2 而不是 1：集成套件的一条用例在等子进程/端口/pty 时另一条可以推进，
    代价是空转（不烧 CPU）。``stream_render_reconcile`` 这种"用例不多但每条重"的套件
    （11 条用例，1 条线程 8.6s、2 条 5.3s）就靠这条下界活下来。

    为什么不是 ``min(tests, cores)``（"用例数够多就给满线程"）：那样只要用例数 ≥ 核数
    就给满线程，池子里线程数会到核数的平方量级（12 核上峰值上百条），把机器压实到 3×
    以上；换来的墙钟收益全被大套件的降速吃掉。
    """
    share = cores * tests / max(1, total_tests)
    return max(min(_THIN_FLOOR, cores), min(cores, round(share)))


def _count_tests(suite: Suite) -> int:
    """``--list --format terse`` 数用例；数不出来（二进制跑不起来）就当 1。

    探测失败不该让整轮测试挂掉：套件照常跑（用例数按 1 算 → 排到最后、按下界线程），
    真有问题会在它的输出里现形，而不是在调度阶段变成一条与本套件无关的报错。
    """
    try:
        proc = subprocess.run(
            [suite.executable, "--list", "--format", _LIST_FORMAT],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return 1
    if proc.returncode != 0:
        return 1
    return max(
        1, sum(1 for line in proc.stdout.splitlines() if line.endswith(": test"))
    )


def _weigh(suites: list[Suite]) -> list[Suite]:
    """给每个套件补上用例数（并发 ``--list``，顺序保持）。"""
    if not suites:
        return []
    with ThreadPoolExecutor(max_workers=min(_LIST_WORKERS, len(suites))) as pool:
        counts = list(pool.map(_count_tests, suites))
    return [replace(suite, tests=count) for suite, count in zip(suites, counts)]


def _label_suites(found: list[tuple[str, str, str]]) -> list[Suite]:
    """``(name, kind, executable)`` → 去重后的套件表（撞名时带上 target kind）。"""
    names = [name for name, _, _ in found]
    suites: list[Suite] = []
    seen: set[str] = set()
    for name, kind, executable in found:
        if executable in seen:
            continue
        seen.add(executable)
        label = f"{name} ({kind})" if names.count(name) > 1 else name
        suites.append(Suite(label=label, executable=executable, tests=0))
    return suites


def build_suites() -> tuple[list[Suite], int]:
    """``cargo test --no-run`` 构建并返回 ``(套件表, cargo 退出码)``。

    ``--message-format=json`` 把 rustc 诊断搬到 **stdout**（``compiler-message``），
    stderr 只剩 cargo 的摘要行——失败时把 ``rendered`` 打回 stderr，否则构建失败
    退化成"知道挂了、不知道为什么"，得再跑一遍裸 ``cargo test``。
    """
    proc = subprocess.run(
        [
            "cargo",
            "test",
            "--workspace",
            "--no-run",
            "--message-format=json",
        ],
        stdout=subprocess.PIPE,
        text=True,
    )
    found: list[tuple[str, str, str]] = []
    for line in proc.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") == "compiler-message":
            rendered = message.get("message", {}).get("rendered")
            if rendered:
                print(
                    rendered,
                    file=sys.stderr,
                    end="" if rendered.endswith("\n") else "\n",
                )
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        if not message.get("profile", {}).get("test"):
            continue
        executable = message.get("executable")
        if not executable:
            continue
        target = message["target"]
        found.append((target["name"], "+".join(target.get("kind") or []), executable))
    return _label_suites(found), proc.returncode


def run_suite(suite: Suite, threads: int) -> Result:
    started = time.monotonic()
    proc = subprocess.run(
        [suite.executable, "--test-threads", str(threads)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return Result(
        suite=suite,
        code=proc.returncode,
        elapsed=time.monotonic() - started,
        output=proc.stdout or "",
        threads=threads,
    )


def run_doctests() -> Result:
    """``cargo test --workspace --doc``：doctests 不在 ``--no-run`` 的产物里，单独跑一次。

    只跑**一次**：``cargo test --workspace``（不带 ``--no-run``）本来就会跑 doctests，
    再叠一条 ``--doc`` 等于跑两遍（旧 ``make test-rust`` 的两行命令就是这个形状）。

    它是一条独立的 cargo 调用（自己还要编译 rustdoc 的用例），所以**第一个**提交给
    池子：它 2~4s，排在 55 个套件后面就是无条件多出来的尾部长。源码有改动时它的编译会
    与测试池并发（热构建只剩几毫秒），各套件的 elapsed 因此可能含一点编译竞争。
    """
    started = time.monotonic()
    proc = subprocess.run(
        ["cargo", "test", "--workspace", "--doc"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return Result(
        suite=Suite(label="doctests", executable="cargo test --doc", tests=0),
        code=proc.returncode,
        elapsed=time.monotonic() - started,
        output=proc.stdout or "",
    )


def _result_lines(output: str) -> list[str]:
    """libtest 的汇总行（``test result: …``）—— 每二进制恰好一行。"""
    return [line for line in output.splitlines() if line.startswith("test result:")]


USAGE = "[--cores N|--serial]"


def _parse_args(argv: list[str]) -> int | None:
    """解析并行度参数（非法值给 usage，不抛 traceback）。"""
    cores = _default_cores()
    args = list(argv)
    while args:
        head = args.pop(0)
        if head == "--serial":
            cores = 1
            continue
        if head == "--cores":
            if not args:
                return None
            raw = args.pop(0)
            if not raw.isdigit() or int(raw) < 1:
                return None
            cores = int(raw)
            continue
        return None
    return cores


def main(argv: list[str]) -> int:
    if argv[1:] == ["--selftest"]:
        return _run_selftest()
    cores = _parse_args(argv[1:])
    if cores is None:
        print(f"usage: {argv[0]} {USAGE}", file=sys.stderr)
        return 2

    started = time.monotonic()
    suites, cargo_code = build_suites()
    if cargo_code != 0:
        return cargo_code
    suites = _weigh(suites)
    build_elapsed = time.monotonic() - started
    if not suites:
        print("no test binaries found", file=sys.stderr)
        return 1

    # 最长先跑：用例数多的套件先发车，短套件填在它的空档里。
    scheduled = sorted(suites, key=lambda suite: suite.tests, reverse=True)
    total_tests = sum(suite.tests for suite in scheduled)
    plan = [
        (suite, _threads_for(suite.tests, total_tests, cores)) for suite in scheduled
    ]
    peak = sum(threads for _, threads in plan[:cores])
    print(
        f"▶ {len(plan)} test binaries (+doctests), {total_tests} listed tests, "
        f"cores={cores} (≤{peak} threads in flight, build {build_elapsed:.1f}s)"
    )

    lock = threading.Lock()
    results: list[Result] = []

    def report(future: Future[Result]) -> None:
        # 回调跑在 worker 线程里：结果与打印都在锁内落地。异常也要收成失败记录，
        # 否则「二进制起不来」（例如被杀）会被静默当成通过。
        with lock:
            try:
                result = future.result()
            except Exception as exc:  # pragma: no cover - 只在不该发生时才走
                print(f"❌ suite crashed: {exc}", file=sys.stderr)
                results.append(
                    Result(
                        suite=Suite(label="<crashed>", executable="", tests=0),
                        code=1,
                        elapsed=0.0,
                        output=str(exc),
                    )
                )
                return
            results.append(result)
            status = "ok" if result.code == 0 else f"FAILED (exit {result.code})"
            lanes = "cargo" if result.threads is None else f"{result.threads}T"
            print(
                f"     Running {result.suite.label} … {result.elapsed:.1f}s "
                f"[{lanes}] {status}"
            )
            # 失败套件的汇总行留给下面的完整输出：每条 `test result:` 只能出现一次，
            # 否则 collect_output.sh 的求和会双重计数。
            if result.code == 0:
                for line in _result_lines(result.output):
                    print(line)

    with ThreadPoolExecutor(max_workers=cores) as pool:
        # doctests 先发：它是一条独立的 cargo 调用（2~4s），排在队尾就是净多出来的长尾。
        futures = [pool.submit(run_doctests)]
        futures += [pool.submit(run_suite, suite, threads) for suite, threads in plan]
        for future in futures:
            future.add_done_callback(report)

    failed = [result for result in results if result.code != 0]
    elapsed = time.monotonic() - started
    if not failed:
        print(f"✅ {len(results)} suites passed in {elapsed:.1f}s")
        return 0

    for result in failed:
        print(f"\n───── {result.suite.label} 失败（exit {result.code}）完整输出 ─────")
        print(result.output)
        print(f"───── {result.suite.label} 输出结束 ─────")
    print(
        f"\n❌ {len(failed)} suite(s) failed: "
        f"{', '.join(r.suite.label for r in failed)}"
    )
    return 1


@contextlib.contextmanager
def _env(**values: str | None) -> Iterator[None]:
    """临时设定 / 清除若干环境变量（`--selftest` 用来把环境变成可控输入）。"""
    saved = {name: os.environ.get(name) for name in values}
    try:
        for name, value in values.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        yield
    finally:
        for name, value in saved.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value


def _run_selftest() -> int:
    """离线自检三条调度规则（与 ``scripts/ci/patch_coverage.py --selftest`` 同一个惯例）。

    规则本身是这一层唯一的"逻辑"，而它的效果只能靠跑一整轮真实测试间接观察——预算 1、
    预算小于下界、``--list`` 失败的二进制都不会在全量跑里现形。
    """
    cases = [
        # (tests, total_tests, cores) -> threads
        ((1, 3759, 12), 2),
        ((0, 3759, 12), 2),
        ((3133, 3759, 12), 10),
        ((17, 3759, 12), 2),
        ((3133, 3759, 1), 1),
        ((3133, 3759, 2), 2),
        ((1, 0, 12), 12),
    ]
    failures = 0
    for (tests, total_tests, cores), want in cases:
        got = _threads_for(tests, total_tests, cores)
        ok = got == want
        failures += 0 if ok else 1
        print(
            f"[selftest] {'ok  ' if ok else 'FAIL'} threads({tests}, {total_tests}, {cores}) = {got}"
            f"{'' if ok else f' (want {want})'}"
        )

    for argv, want in [
        ([], _cpu_count()),
        (["--serial"], 1),
        (["--cores", "4"], 4),
        (["--cores", "0"], None),
        (["--cores"], None),
        (["--jobs", "8"], None),
        (["--bogus"], None),
    ]:
        # 环境是输入的一部分：把两个开关清掉再断言"没有旋钮时按独占算"，
        # 否则 `WING_TEST_BUDGET_RUST=4 make test` 里跑 `--selftest` 会假红。
        with _env(**{SERIAL_ENV: None, BUDGET_ENV: None}):
            got = _parse_args(argv)
        ok = got == want
        failures += 0 if ok else 1
        print(
            f"[selftest] {'ok  ' if ok else 'FAIL'} parse{argv} = {got}{'' if ok else f' (want {want})'}"
        )

    for env, want in [
        ({SERIAL_ENV: "1", BUDGET_ENV: "4"}, 1),
        ({SERIAL_ENV: None, BUDGET_ENV: "3"}, 3),
        ({SERIAL_ENV: None, BUDGET_ENV: " 2 "}, 2),
        ({SERIAL_ENV: None, BUDGET_ENV: ""}, _cpu_count()),
        ({SERIAL_ENV: None, BUDGET_ENV: "0"}, _cpu_count()),
        ({SERIAL_ENV: None, BUDGET_ENV: "two"}, _cpu_count()),
        ({SERIAL_ENV: None, BUDGET_ENV: None}, _cpu_count()),
    ]:
        with _env(**env):
            got = _default_cores()
        ok = got == want
        failures += 0 if ok else 1
        print(
            f"[selftest] {'ok  ' if ok else 'FAIL'} default_cores({env}) = {got}"
            f"{'' if ok else f' (want {want})'}"
        )

    print(f"[selftest] {'ok' if not failures else f'{failures} failure(s)'}")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
