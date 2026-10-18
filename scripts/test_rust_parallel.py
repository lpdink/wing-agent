#!/usr/bin/env python3
"""``cargo test --workspace`` 的跨二进制并行版本 —— ``make test-rust`` 的实现。

cargo 逐个串行执行测试二进制（只有**单个二进制内部**才多线程），于是 53 个套件的
耗时直接相加：本机实测 41s 的总和里最长的一个只有 11.5s，其余全是 1~5s 的集成
测试叠出来的长尾。这里复用 ``cargo test --no-run`` 的构建产物，按并发上限同时跑
多个二进制，把长尾摊平。

**输出契约**（``scripts/collect_output.sh`` 依赖它）：每条 ``test result: …`` 在
日志里**恰好出现一次**（通过的套件在完成时打印，失败的套件只出现在完整输出里），
统计行按套件求和。构建失败时 rustc 诊断按 cargo 的渲染形式打印，退出码用 cargo 的。

并行度：``-j/--jobs``（同时几个二进制，默认半个核数）、``-t/--threads``（每个套件
的 libtest 线程数，默认 4）；``WING_TEST_SERIAL=1`` 与另外两组同义（→ 1/1），显式
参数优先。

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

#: 与 ``scripts/collect_output.sh`` 的逃生舱同名：排查并发干扰时四组一起退化成串行。
SERIAL_ENV = "WING_TEST_SERIAL"


@dataclass(frozen=True)
class Suite:
    """一个测试二进制（libtest 的构建产物）。

    ``label`` 是日志里的显示名：lib 单测与同名 bin 单测（如 ``wing``）会撞名，
    撞名时补上 target kind，免得失败清单看起来像重复跑了同一套件。
    """

    label: str
    executable: str


@dataclass(frozen=True)
class Result:
    suite: Suite
    code: int
    elapsed: float
    output: str


def _cpu_count() -> int:
    return os.cpu_count() or 4


def _serial() -> bool:
    """``WING_TEST_SERIAL=1``（非空且非 0）——与 python / probe 两组的判定一致。"""
    value = os.environ.get(SERIAL_ENV, "")
    return bool(value) and value != "0"


def _default_jobs() -> int:
    """同时跑的二进制个数：半个核数（串行逃生舱下为 1）。

    并行度加在**套件之间**收益最大——套件大多是 e2e（pty / 端口 / 子进程），单个
    套件内部的测试本来就在等 I/O，多给线程也吃不掉这些等待。
    """
    return 1 if _serial() else max(2, _cpu_count() // 2)


def _default_threads() -> int:
    """每个二进制的 libtest 线程数（串行逃生舱下为 1）。

    例外是那个 CPU 大头：`wing` 的 2854 个单测（本机占 41s 总和里的 11.5s 墙钟、
    ~110s CPU）。给它 2 线程它就独自变成尾部长（实测该套件 31s → 22s、整体
    36s → 25s，10 核）；空转的小套件不在乎多分线程。
    """
    return 1 if _serial() else 4


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
        suites.append(Suite(label=label, executable=executable))
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
        suite=Suite(label="doctests", executable="cargo test --doc"),
        code=proc.returncode,
        elapsed=time.monotonic() - started,
        output=proc.stdout or "",
    )


def _result_lines(output: str) -> list[str]:
    """libtest 的汇总行（``test result: …``）—— 每二进制恰好一行。"""
    return [line for line in output.splitlines() if line.startswith("test result:")]


USAGE = "[--jobs N|--serial] [--threads N]"


def _parse_args(argv: list[str]) -> tuple[int, int] | None:
    """解析并行度参数（非法值给 usage，不抛 traceback）。"""
    jobs = _default_jobs()
    threads = _default_threads()
    args = list(argv)
    while args:
        head = args.pop(0)
        if head == "--serial":
            jobs = threads = 1
            continue
        if head in ("--jobs", "-j", "--threads", "-t"):
            if not args:
                return None
            raw = args.pop(0)
            if not raw.isdigit() or int(raw) < 1:
                return None
            if head in ("--jobs", "-j"):
                jobs = int(raw)
            else:
                threads = int(raw)
            continue
        return None
    return jobs, threads


def main(argv: list[str]) -> int:
    parsed = _parse_args(argv[1:])
    if parsed is None:
        print(f"usage: {argv[0]} {USAGE}", file=sys.stderr)
        return 2
    jobs, threads = parsed

    started = time.monotonic()
    suites, cargo_code = build_suites()
    if cargo_code != 0:
        return cargo_code
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
                        suite=Suite(label="<crashed>", executable=""),
                        code=1,
                        elapsed=0.0,
                        output=str(exc),
                    )
                )
                return
            results.append(result)
            status = "ok" if result.code == 0 else f"FAILED (exit {result.code})"
            print(f"     Running {result.suite.label} … {result.elapsed:.1f}s {status}")
            # 失败套件的汇总行留给下面的完整输出：每条 `test result:` 只能出现一次，
            # 否则 collect_output.sh 的求和会双重计数。
            if result.code == 0:
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
        print(f"\n───── {result.suite.label} 失败（exit {result.code}）完整输出 ─────")
        print(result.output)
        print(f"───── {result.suite.label} 输出结束 ─────")
    print(
        f"\n❌ {len(failed)} suite(s) failed: "
        f"{', '.join(r.suite.label for r in failed)}"
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
