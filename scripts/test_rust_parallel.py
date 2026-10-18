#!/usr/bin/env python3
"""``cargo test --workspace`` 的跨二进制并行版本 —— ``make test-rust`` 的实现。

cargo 逐个串行执行测试二进制（只有**单个二进制内部**才多线程），于是 53 个套件的
耗时直接相加：本机实测 41s 的总和里最长的一个只有 11.5s，其余全是 1~5s 的集成
测试叠出来的长尾。这里复用 ``cargo test --no-run`` 的构建产物，按并发上限同时跑
多个二进制，把长尾摊平。

**输出契约**（``scripts/collect_output.sh`` 依赖它）：每个二进制打印一行 libtest
原样的 ``test result: …``，统计行按套件求和；失败二进制的完整输出贴在结论之前，
最后列出失败套件并以 1 退出。构建失败时 cargo 的 stderr 原样透传（退出码同 cargo）。

单套件 / 名称过滤请直接用 ``cargo test``：这里只服务"全量跑一遍"。
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass


@dataclass(frozen=True)
class Suite:
    """一个测试二进制（libtest 的构建产物）。"""

    name: str
    executable: str


def _cpu_count() -> int:
    return os.cpu_count() or 4


def _default_jobs() -> int:
    """同时跑的二进制个数：半个核数。

    并行度加在**套件之间**收益最大——套件大多是 e2e（pty / 端口 / 子进程），单个
    套件内部的测试本来就在等 I/O，多给线程也吃不掉这些等待。
    """
    return max(2, _cpu_count() // 2)


def _default_threads() -> int:
    """每个二进制的 libtest 线程数。

    例外是那个 CPU 大头：`wing` 的 2854 个单测（本机占 41s 总和里的 11.5s 墙钟、
    ~110s CPU）。给它 2 线程它就独自变成尾部长（实测该套件 31s → 22s、整体
    36s → 25s，10 核）；空转的小套件不在乎多分线程。
    """
    return 4


def build_suites() -> list[Suite] | None:
    """``cargo test --no-run`` 构建并返回去重后的测试二进制（构建失败返回 None）。"""
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
    if proc.returncode != 0:
        return None
    suites: list[Suite] = []
    seen: set[str] = set()
    for line in proc.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        if not message.get("profile", {}).get("test"):
            continue
        executable = message.get("executable")
        if not executable or executable in seen:
            continue
        seen.add(executable)
        suites.append(Suite(name=message["target"]["name"], executable=executable))
    return suites


@dataclass(frozen=True)
class Result:
    suite: Suite
    code: int
    elapsed: float
    output: str


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
    )


def run_doctests() -> Result:
    """``cargo test --workspace --doc``：doctests 不在 ``--no-run`` 的产物里，单独跑一次。

    只跑**一次**：``cargo test --workspace``（不带 ``--no-run``）本来就会跑 doctests，
    再叠一条 ``--doc`` 等于跑两遍（旧 ``make test-rust`` 的两行命令就是这个形状）。
    """
    started = time.monotonic()
    proc = subprocess.run(
        ["cargo", "test", "--workspace", "--doc"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return Result(
        suite=Suite(name="doctests", executable="cargo test --doc"),
        code=proc.returncode,
        elapsed=time.monotonic() - started,
        output=proc.stdout or "",
    )


def _result_lines(output: str) -> list[str]:
    """libtest 的汇总行（``test result: …``）—— 每二进制恰好一行。"""
    return [line for line in output.splitlines() if line.startswith("test result:")]


def main(argv: list[str]) -> int:
    jobs = _default_jobs()
    threads = _default_threads()
    args = list(argv[1:])
    while args:
        head = args.pop(0)
        if head in ("--jobs", "-j") and args:
            jobs = int(args.pop(0))
        elif head in ("--threads", "-t") and args:
            threads = int(args.pop(0))
        else:
            print(f"usage: {argv[0]} [--jobs N] [--threads N]", file=sys.stderr)
            return 2

    started = time.monotonic()
    suites = build_suites()
    if suites is None:
        print("cargo test --no-run failed", file=sys.stderr)
        return 1
    build_elapsed = time.monotonic() - started
    if not suites:
        print("no test binaries found", file=sys.stderr)
        return 1

    print(
        f"▶ {len(suites)} test binaries (+doctests), "
        f"jobs={jobs} threads={threads} (build {build_elapsed:.1f}s)"
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
                        suite=Suite(name="<crashed>", executable=""),
                        code=1,
                        elapsed=0.0,
                        output=str(exc),
                    )
                )
                return
            results.append(result)
            status = "ok" if result.code == 0 else f"FAILED (exit {result.code})"
            print(f"     Running {result.suite.name} … {result.elapsed:.1f}s {status}")
            for line in _result_lines(result.output):
                print(line)

    with ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = [pool.submit(run_suite, suite, threads) for suite in suites]
        futures.append(pool.submit(run_doctests))
        for future in futures:
            future.add_done_callback(report)

    failed = [result for result in results if result.code != 0]
    elapsed = time.monotonic() - started
    if not failed:
        print(f"✅ {len(results)} suites passed in {elapsed:.1f}s")
        return 0

    for result in failed:
        print(f"\n───── {result.suite.name} 失败（exit {result.code}）完整输出 ─────")
        print(result.output)
        print(f"───── {result.suite.name} 输出结束 ─────")
    print(
        f"\n❌ {len(failed)} suite(s) failed: {', '.join(r.suite.name for r in failed)}"
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
