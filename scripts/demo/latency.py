#!/usr/bin/env python3
"""端到端**显示延迟**水位尺：Provider 每 N 帧插一行 ``⟦M#####⟧``，在真 TUI 的
pty 字节流里找它首次出现 → ``lag = 出现在终端 − Provider 发射``。

量的是用户真正感受到的那条链：Provider → 网关 → WS → TUI 读任务 → 事件通道 →
帧闸门(16ms) → 终端写出。标记独占一行（短行不折行），所以字节流里必然连续。

    uv run python scripts/demo/latency.py                      # 默认阶梯
    uv run python scripts/demo/latency.py --steps 3000,30000 --seconds 8
    uv run python scripts/demo/latency.py --steps 3000 --json /tmp/lat.json

怎么读：
* ``p50/p99`` 是每个标记帧"从发射到画进终端"的时间；稳态下它不该随台阶上涨；
* ``趋势`` = 后四分之一段的 p50 − 前四分之一段的 p50。持续为正 = 开始积压
  （上游 TCP 反压会一路传回 Provider，Provider 侧的时间戳也已经漂了）。

``--json`` 只是给脚本/流水线加的一路**机器可读**输出（``{meta, measurements[]}``，与表格
同源同口径，非有限值写作 ``null``）；不带它时行为与从前一字不差。perf 的 A/B 套件
（``scripts/perf/suite_tui.py``）在进程内调用 :func:`run_rate`，不经过 CLI。

与 `lag_marker.py`（fast-stream 时代的 CTE 工具）同源；这里接的是本仓库自己的
`stream.py`（同一份语料、同一个 deadline 自校正发射器），所以 README 里引的性能
数字可以用仓库里的东西复算。
"""

from __future__ import annotations

import argparse
import fcntl
import json
import math
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import threading
import time
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))

import env  # noqa: E402  (scripts/demo 内部共用模块：ready 行解析等)

#: 探针代答的终端查询：真终端自己会答，PTY 里没人答，不答 TUI 首帧永远不来。
REPLIES = {b"\x1b[6n": b"\x1b[1;1R", b"\x1b[c": b"\x1b[?62;1;6;22c"}
#: 探针里终端多大（列 × 行）——与录制无关，只要装得下标记行。
PROBE_COLS, PROBE_ROWS = 120, 45
#: 喂的内容：0 = 全正文（markdown + 代码块 + 表格，渲染**最贵**的那条路径）。
#: 量的是"用户真实会遇到的最坏情况"，所以不走便宜的纯 reasoning 流。
REASON_CHARS = 0
#: 标记搜索前要剥掉的终端控制序列（SGR / CSI / OSC）：行是分多次写出的，
#: 中间会夹 CUP 与颜色，直接在原始字节里找 needle 会漏掉一半。
ANSI_RE = re.compile(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[ -/]*[@-~]")
#: 找到的标记占比低于这个值就认为这一档"样本不足"，不进结论。
MIN_COVERAGE = 0.4
#: ``resend_after`` 生效时，同一档最多补发几次 prompt（见 :func:`run_rate`）。
RESEND_LIMIT = 3


def cpu_seconds(pid: int) -> float:
    """进程累计 CPU 时间（秒）。ps 的 time 字段是 mm:ss.ss / hh:mm:ss，手动解析。"""
    result = subprocess.run(
        ["ps", "-o", "time=", "-p", str(pid)],
        capture_output=True,
        text=True,
        check=False,
    )
    raw = result.stdout.strip()
    if not raw:
        return float("nan")
    parts = [float(p) for p in raw.split(":")]
    total = 0.0
    for part in parts:
        total = total * 60 + part
    return total


def wing_bin() -> Path:
    for key in ("WING_BIN", "DEMO_WING_BIN"):
        if os.environ.get(key):
            return Path(os.environ[key])
    for candidate in (REPO / "target/release/wing", REPO / "target/debug/wing"):
        if candidate.is_file():
            return candidate
    raise SystemExit(
        "[latency] no wing binary — cargo build --release -p wing, or set $WING_BIN"
    )


class Provider:
    """跑一个 `stream.py`（假 Provider + 网关）子进程，读它的 ready 行。"""

    def __init__(self, tps: float, emit_log: Path, marker_every: int) -> None:
        self.emit_log = emit_log
        self.proc = subprocess.Popen(
            [
                sys.executable,
                "-u",
                str(HERE / "stream.py"),
                f"--tps={tps}",
                f"--marker-every={marker_every}",
                f"--emit-log={emit_log}",
                "--ttft=0.05",
                f"--reason-chars={REASON_CHARS}",
                # 语料给足：一个台阶里不能把回合跑完（跑完了就没得量了）
                "--corpus-bytes=4000000",
                "--turn-chars=4000000",
            ],
            cwd=str(REPO),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        self.info: dict[str, str] = {}

        def pump() -> None:
            assert self.proc.stdout is not None
            for line in self.proc.stdout:
                ready = env.parse_ready(line)
                if ready:
                    self.info.update(ready)

        threading.Thread(target=pump, daemon=True).start()
        deadline = time.monotonic() + 45
        while "WING_GATEWAY_PORT" not in self.info and time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise SystemExit("[latency] stream.py exited early")
            time.sleep(0.1)
        if "WING_GATEWAY_PORT" not in self.info:
            raise SystemExit("[latency] stream.py never became ready")

    def stop(self) -> None:
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def run_rate(
    rate: int,
    seconds: float,
    marker_every: int,
    keep: bool,
    *,
    emit_path: Path | None = None,
    resend_after: float | None = None,
) -> dict[str, float]:
    """跑一个台阶：起 provider + 网关，PTY 里跑真 TUI，量标记的显示延迟。

    ``emit_path`` 是 provider 每帧发射时刻 JSONL 的落点（``None`` = 既有的
    ``/tmp/wing-latency-<rate>.jsonl``）；perf A/B 套件用它把中间产物隔离到自己的
    scratch。返回值是这一档的 row（字段与表格列一一对应，时间单位是**秒**）。

    ``resend_after``（可选，默认 ``None`` = 不补发，与从前一字不差）：首击后 N 秒
    provider 还是**一帧都没发出来**（emit log 为空），就再送一次同一句 prompt，最多
    ``RESEND_LIMIT`` 次。动机见 perf-ci 04 的 design.md：TUI 在启动期的终端查询阶段会
    冲掉先到的按键，首击偶尔整个丢掉（网关侧证据：一次 ``routing 'go'`` 都没有），而
    6s 档位经不起整档白测。守卫是"provider 零帧"这个**代理信号**（不是"prompt 未被消费"
    的直接证据）：只要第一帧流出就立刻停止补发——实测"网关开始路由 → provider 首帧"
    这一跳约 60ms，对 0.7s 的套件取值有 10x+ 余量；把阈值压到 10ms 时确实可能重复投递
    （重复的消息只进会话 inbox，不形成第二个可见轮次，不污染被测指标）。
    """
    reply_log: list[bytes] = []
    #: 待答的终端查询：``[应答时刻, 排队时的输出字节数, 已试次数]``。
    #: crossterm 读光标位置前会冲掉已到达的输入 —— 回包抢在它冲之前就白给，
    #: 应用会卡满 2s 超时（TUI 日志里那条 `cursor position could not be read`）。
    #: 所以"没人继续画"就补发，看到输出增长就撤掉。
    pending: list[list[float]] = []
    emit = (
        emit_path
        if emit_path is not None
        else Path("/tmp") / f"wing-latency-{rate}.jsonl"
    )
    emit.parent.mkdir(parents=True, exist_ok=True)

    def emit_idle() -> bool:
        """provider 还没发过任何一帧（emit log 为空）；读不到就当"非空"（不补发）。"""
        try:
            return emit.stat().st_size == 0
        except OSError:
            return False

    provider = Provider(rate, emit, marker_every)
    try:
        env = dict(os.environ)
        env.update(
            WING_HOME=provider.info["WING_HOME"],
            TERM="xterm-256color",
            COLORTERM="truecolor",
        )
        pid, fd = pty.fork()
        if pid == 0:
            binary = str(wing_bin())
            gateway = provider.info["WING_GATEWAY_PORT"]
            os.execve(
                binary,
                [binary, "tui", "--host", "127.0.0.1", "--port", gateway],
                env,
            )
        fcntl.ioctl(
            fd, termios.TIOCSWINSZ, struct.pack("HHHH", PROBE_ROWS, PROBE_COLS, 0, 0)
        )

        chunks: list[tuple[float, bytes]] = []
        started = time.perf_counter()
        typed = False
        typed_at = 0.0
        resends = 0
        total_bytes = 0
        cpu_start = cpu_at = float("nan")
        # 滚动窗口：等 TUI 真的画出首屏（欢迎屏上的提示行）再送 prompt。固定 sleep
        # 会在高负载下抢跑——keystroke 落进还没开始读 stdin 的进程，整档就白测。
        recent = bytearray()
        carry = b""
        try:
            while time.perf_counter() - started < seconds:
                ready, _, _ = select.select([fd], [], [], 0.1)
                if ready:
                    try:
                        data = os.read(fd, 1 << 20)
                    except OSError:
                        break
                    if not data:
                        break
                    chunks.append((time.perf_counter(), data))
                    total_bytes += len(data)
                    # 查询只有 4 字节，可能正好被 read 切开：拿"上一次的尾巴 + 本次"
                    # 一起找，减去已在尾巴里数过的，避免重复应答。
                    window = carry + data
                    for needle, reply in REPLIES.items():
                        for _ in range(window.count(needle) - carry.count(needle)):
                            pending.append([time.perf_counter(), total_bytes, 0, reply])
                    carry = data[-8:]
                    recent.extend(ANSI_RE.sub(b"", data))
                    del recent[:-8192]

                # 补答：应用没继续画（输出没长）就隔 200ms 再答一次，最多 15 次；
                # 一旦看到它继续画，说明这条回包被读到了，撤销。
                for item in list(pending):
                    due, mark, tries, reply = item
                    if total_bytes > mark + 200 or tries >= 15:
                        pending.remove(item)
                    elif time.perf_counter() >= due:
                        os.write(fd, reply)
                        reply_log.append(reply)
                        item[0] = time.perf_counter() + 0.2
                        item[2] = tries + 1
                elapsed = time.perf_counter() - started
                if not typed and (b"Esc " in recent or elapsed > 8.0):
                    os.write(fd, b"go\r")
                    typed = True
                    typed_at = time.perf_counter()
                    cpu_start = cpu_seconds(pid)
                if (
                    resend_after is not None
                    and typed
                    and resends < RESEND_LIMIT
                    and time.perf_counter() - typed_at > resend_after
                    and emit_idle()
                ):
                    os.write(fd, b"go\r")
                    resends += 1
                    typed_at = time.perf_counter()
                    cpu_start = cpu_seconds(pid)
                if typed and cpu_start == cpu_start:
                    cpu_at = cpu_seconds(pid)
        finally:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.kill(pid, sig)
                    time.sleep(0.3)
                except ProcessLookupError:
                    break
            try:
                os.close(fd)
            except OSError:
                pass
    finally:
        provider.stop()

    # Provider 侧：每个标记帧的发射时刻（marker id = seq // marker_every）
    sent: dict[int, float] = {}
    with emit.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            seq = int(record.get("seq", -1))
            if seq >= 0 and seq % marker_every == 0:
                sent.setdefault(seq // marker_every, float(record["sent"]))
    if not keep:
        emit.unlink(missing_ok=True)

    if os.environ.get("LATENCY_DEBUG"):
        total = sum(len(data) for _at, data in chunks)
        tail = ANSI_RE.sub(b"", chunks[-1][1])[-120:] if chunks else b""
        head = chunks[0][1][:160] if chunks else b""
        print(
            f"[debug] chunks={len(chunks)} bytes={total} emitted={len(sent)} "
            f"replies={len(reply_log)} typed={typed} "
            f"head={head!r} tail={tail!r}",
            file=sys.stderr,
        )

    # TUI 侧：标记首次出现在哪个读分片里。标记可能正好被 os.read 切开，所以每片
    # 都拼上前一片的尾巴再找（这与查询应答用的是同一套 carry 思路）。
    padded: list[tuple[float, bytes]] = []
    prev = b""
    for at, data in chunks:
        padded.append((at, prev + data))
        prev = data[-24:]
    lag: list[float] = []
    missing = 0
    for marker_id, sent_at in sorted(sent.items()):
        needle = f"⟦M{marker_id:05d}".encode()
        found = next(
            (at for at, data in padded if needle in ANSI_RE.sub(b"", data)), None
        )
        if found is None:
            missing += 1
        else:
            lag.append(found - sent_at)

    def pct(values: list[float], q: float) -> float:
        if not values:
            return float("nan")
        ordered = sorted(values)
        return ordered[min(len(ordered) - 1, int(len(ordered) * q))]

    quarter = max(1, len(lag) // 4)
    window = (
        0.0
        if cpu_at != cpu_at or cpu_start != cpu_start
        else max(0.0, cpu_at - cpu_start)
    )
    elapsed = max(0.001, time.perf_counter() - (typed_at or started))
    coverage = len(lag) / len(sent) if sent else 0.0
    return {
        "rate": rate,
        "coverage": coverage,
        "cpu": 100.0 * window / elapsed
        if elapsed > 0 and window == window
        else float("nan"),
        "markers": len(sent),
        "missing": missing,
        "min": min(lag) if lag else float("nan"),
        "p50": pct(lag, 0.5),
        "p99": pct(lag, 0.99),
        "max": max(lag) if lag else float("nan"),
        "trend": (pct(lag[-quarter:], 0.5) - pct(lag[:quarter], 0.5))
        if lag
        else float("nan"),
        #: 首击被启动期吃掉时的补发次数（``resend_after`` 为 None 时恒 0）——
        #: 表格不显示；它进 --json 与 suite 的 meta，是"这一档为什么起得晚"的证据。
        "resends": resends,
    }


def json_ready(value: Any) -> Any:
    """JSON 化一个 row 字段：非有限的浮点写作 ``None``（NaN/Infinity 不是合法 JSON）。

    表格里 NaN 显示成 ``nanm``、JSON 里显示成 ``null`` —— 同一个事实的两种表达。
    """
    if isinstance(value, float) and not math.isfinite(value):
        return None
    return value


def write_json_report(
    path: Path,
    rows: Sequence[Mapping[str, float]],
    *,
    seconds: float,
    marker_every: int,
    steps: Sequence[int],
) -> None:
    """把每次测量的 row 与 meta 原子写成 JSON（``--json``；不影响 stdout 的表格）。"""
    payload = {
        "meta": {
            "seconds": seconds,
            "marker_every": marker_every,
            "steps": list(steps),
            "wing_bin": str(wing_bin()),
        },
        "measurements": [
            {key: json_ready(value) for key, value in row.items()} for row in rows
        ],
    }
    text = json.dumps(payload, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    tmp = path.with_name(path.name + ".tmp")
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)
    print(f"[latency] wrote {path}", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="end-to-end display latency of the TUI"
    )
    parser.add_argument(
        "--steps", default="3000,30000,45000", help="逗号分隔的 tok/s 阶梯"
    )
    parser.add_argument("--seconds", type=float, default=8.0, help="每个台阶测多久")
    parser.add_argument(
        "--marker-every", type=int, default=400, help="每 N 帧插一个标记"
    )
    parser.add_argument("--keep", action="store_true", help="保留各台阶的 emit log")
    parser.add_argument(
        "--json",
        help=("把每次测量的行数据与 meta 写成 JSON（与表格同口径，非有限值写 null）"),
    )
    args = parser.parse_args()

    steps = [int(x) for x in args.steps.split(",")]
    print(f"标记每 {args.marker_every} 帧一行；lag = 标记出现在终端 − Provider 发射")
    print(
        f"{'帧/s':>9}{'标记':>7}{'命中率':>8}{'min':>9}{'p50':>9}{'p99':>9}{'max':>9}"
        f"{'趋势':>9}{'TUI CPU':>9}   判定"
    )
    rows: list[dict[str, float]] = []
    for step in steps:
        row = run_rate(step, args.seconds, args.marker_every, args.keep)
        rows.append(row)
        verdict = "跟得上"
        if row["coverage"] < MIN_COVERAGE:
            # 命中率低说明多数标记行在两次绘制之间就被滚过去了（高速下的常态），
            # 这时 p50/p99 只代表"能被逐帧抽到的那部分"，不能当结论用。
            verdict = f"样本不足（命中 {row['coverage'] * 100:.0f}%）"
        elif row["p99"] == row["p99"] and row["p99"] > 0.25:
            verdict = "开始落后"
        if (
            row["coverage"] >= MIN_COVERAGE
            and row["trend"] == row["trend"]
            and row["trend"] > 0.1
        ):
            verdict += " + 持续拉大"
        print(
            f"{row['rate']:>9}{row['markers']:>7}{row['coverage'] * 100:>7.0f}%"
            f"{row['min'] * 1000:>8.1f}m{row['p50'] * 1000:>8.1f}m"
            f"{row['p99'] * 1000:>8.1f}m{row['max'] * 1000:>8.1f}m"
            f"{row['trend'] * 1000:>+8.1f}m{row['cpu']:>8.0f}%   {verdict}",
            flush=True,
        )
    if args.json:
        write_json_report(
            Path(args.json),
            rows,
            seconds=args.seconds,
            marker_every=args.marker_every,
            steps=steps,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
