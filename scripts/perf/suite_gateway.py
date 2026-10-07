#!/usr/bin/env python3
"""perf-ci 的 gateway 套件：socket 级（无 TUI）的网关性能测量（05 步骤）。

四个场景各自独立隔离（fresh ``WING_HOME`` + fresh 网关进程 + fresh 假 Provider），
经公开 HTTP / WS 协议驱动**真网关**（``<side>/…/.venv/bin/wing-gateway``）与进程内
假 Provider（``wing_probe``），产出契约 §4 的 8 个指标：

    fanout   1 prompt → 零延迟 N 帧        → complete_ms / gap_p99_us / cpu_ms_per_1k
    turns    K 个迷你轮次                  → turn.p50_ms / turn.p99_ms
    context  大 history + probe prompt     → context.build_p50_ms / build_p99_ms
    resume   重启网关后重新订阅            → resume.sync_ms

调用形态（契约 §2）：

    <python> scripts/perf/suite_gateway.py --side <side.json> --out <out.json> \\
        --round <n> [--quick]

退出码：0 = 产出有效；2 = 参数 / 环境错误（未启动任何进程）；3 = 场景失败
（仍落盘 ``ok=false`` 的产物，供 ab 记 ``failures[]`` 与人工取证）。

时间戳口径、CPU 测量方法与"为什么不用 scripts/demo/stream.py 的发射器"见本步骤
design.md（D2 / D3 / D7）。软异常（帧数 / 请求数对不上、采样不足）一律写
``meta.notes``，不静默吞。
"""

from __future__ import annotations

import argparse
import asyncio
import ctypes
import ctypes.util
import json
import math
import os
import shutil
import struct
import sys
import time
import traceback
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
# harness 代码永远来自 head（rig 恒定，见 01 design D2）：本脚本所在目录给 `common`，
# head 仓库的 wing-probe 给 `wing_probe`（不依赖 venv 的 editable 安装）。
for _entry in (HERE, REPO_ROOT / "libs" / "wing-probe"):
    if str(_entry) not in sys.path:
        sys.path.insert(0, str(_entry))

from common import Side, SuiteOutput, median  # noqa: E402  (同目录模块)
from wing_probe.driver.session import Session  # noqa: E402
from wing_probe.env import ProbeEnv  # noqa: E402
from wing_probe.probe import Probe  # noqa: E402
from wing_probe.provider.script import Script, Turn  # noqa: E402
from wing_probe.watch.timeline import Event  # noqa: E402

# ── 常量 ──────────────────────────────────────────────────────

MODEL_PREFIX = "perf"
"""假 Provider 剧本的 model 名前缀：每个场景一个 model，剧本互不串台。"""

TOOLS: tuple[str, ...] = ("Read", "Glob", "Grep")
"""会话工具集：固定且最小——工具 schema 的体积会摊进每轮请求，属被测变量之外的噪声。"""

SYSTEM_PROMPT = "You are wing's gateway perf suite agent. Reply briefly."
CONTEXT_WINDOW_TOKENS = 256_000
KEEP_RECENT_TOKENS = 50_000

WARMUP_TEXT = "warmup"
"""warmup 轮的 provider 回复（不计入任何样本；见 design D5）。"""

FANOUT_CHUNK_CHARS = 1
"""fanout 每帧的字符数：1 = 一字符一帧，指标量的是**帧数**（1000/3000）而非字节数。"""

TURN_WITHIN = 30.0
"""单个迷你轮次 / 构造轮次的等待上限（秒）。"""

FANOUT_WITHIN = 120.0
"""一次 N 帧扇出的等待上限（秒）——CI 慢机也够。"""

SYNC_WITHIN = 30.0
"""``sync_session`` 重放的等待上限（秒）。"""

HEALTH_TIMEOUT = 60.0
"""网关启动健康检查上限（秒）。"""

SETTLE_S = 0.2
"""被测轮次前的静置：等上一轮的后台收尾（提交 / 日志 flush）落定再开计时窗口。"""

EXPECTED_METRICS: tuple[str, ...] = (
    "gateway.fanout.complete_ms",
    "gateway.fanout.gap_p99_us",
    "gateway.fanout.cpu_ms_per_1k",
    "gateway.turn.p50_ms",
    "gateway.turn.p99_ms",
    "gateway.context.build_p50_ms",
    "gateway.context.build_p99_ms",
    "gateway.resume.sync_ms",
)


class ScenarioError(RuntimeError):
    """场景级硬失败（超时 / error 事件 / 数据不可用）——套件据此判失败。"""


# ── 参数档位（design D8） ─────────────────────────────────────


@dataclass(frozen=True)
class Settings:
    """quick / full 两档的构造参数。"""

    quick: bool
    fanout_frames: int
    mini_turns: int
    history_turns: int
    history_reply_chars: int
    probe_prompts: int
    resume_cycles: int

    def history_reply(self, index: int) -> str:
        """第 ``index`` 个构造轮次的 provider 回复：定长、确定性（字符级规模可控）。"""
        unit = "context filler line for the perf suite\n"
        seed = f"[{index:03d}] "
        body = seed + unit * (self.history_reply_chars // len(unit) + 1)
        return body[: self.history_reply_chars]


def settings_for(*, quick: bool) -> Settings:
    if quick:
        return Settings(
            quick=True,
            fanout_frames=1_000,
            mini_turns=12,
            history_turns=10,
            history_reply_chars=10_000,
            probe_prompts=5,
            resume_cycles=2,
        )
    return Settings(
        quick=False,
        fanout_frames=3_000,
        mini_turns=30,
        history_turns=30,
        history_reply_chars=10_000,
        probe_prompts=8,
        resume_cycles=3,
    )


# ── 数值 ──────────────────────────────────────────────────────


def percentile(values: Sequence[float], pct: float) -> float:
    """nearest-rank 百分位（p99 = 排序后第 ceil(0.99·n) 个值）。

    ``common.median`` 对偶数样本取两数均值，与 nearest-rank 差半个秩；本套件的
    p50/p99 都用同一套秩定义，自洽即可（口径写进设计文档）。
    """
    ordered = sorted(values)
    if not ordered:
        raise ScenarioError("percentile() of an empty sequence")
    rank = max(1, math.ceil(pct / 100.0 * len(ordered)))
    return float(ordered[min(rank, len(ordered)) - 1])


# ── 网关进程 CPU 采样（design D3） ─────────────────────────────

#: ``proc_pid_rusage`` 的返回缓冲区大小。结构随 flavor 增长（v4 已 >200B），而
#: **不能**只给结构前缀的大小——内核按 flavor 写入整块结构，缓冲区小了就是越界写。
_RUSAGE_BUFFER_BYTES = 4096

#: ``ri_user_time`` / ``ri_system_time`` 在 ``rusage_info_v0+`` 结构里的偏移：
#: ``ri_uuid[16]`` 之后两个 ``uint64``（**mach 绝对时钟 tick**，不是纳秒——
#: Apple Silicon 的 timebase 是 125/3，直接除 1e9 会少算 41.7 倍）。
_RUSAGE_TIMES_OFFSET = 16


class _MachTimebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_LIBPROC: Any = None
_MACH_TIMEBASE: tuple[int, int] | None = None


def _mach_timebase() -> tuple[int, int]:
    """``mach_absolute_time`` → 纳秒的换算比（numer, denom）。"""
    global _MACH_TIMEBASE
    if _MACH_TIMEBASE is None:
        info = _MachTimebase()
        rc = int(ctypes.CDLL(None).mach_timebase_info(ctypes.byref(info)))
        if rc != 0 or info.denom == 0:
            raise ScenarioError(
                f"mach_timebase_info failed (rc={rc}, {info.numer}/{info.denom})"
            )
        _MACH_TIMEBASE = (int(info.numer), int(info.denom))
    return _MACH_TIMEBASE


def _libproc() -> Any:
    """加载 ``libproc``（macOS）；失败即抛，绝不静默降级成错数据。"""
    global _LIBPROC
    if _LIBPROC is None:
        path = ctypes.util.find_library("proc") or "/usr/lib/libproc.dylib"
        lib = ctypes.CDLL(path)
        lib.proc_pid_rusage.restype = ctypes.c_int
        lib.proc_pid_rusage.argtypes = [
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_void_p,
        ]
        _LIBPROC = lib
    return _LIBPROC


def _darwin_cpu_seconds(pid: int) -> float:
    buffer = ctypes.create_string_buffer(_RUSAGE_BUFFER_BYTES)
    lib = _libproc()
    rc = -1
    for flavor in (4, 1, 0):  # RUSAGE_INFO_V4 → V1 → V0（前三个字段布局一致）
        rc = int(lib.proc_pid_rusage(pid, flavor, ctypes.byref(buffer)))
        if rc == 0:
            break
    if rc != 0:
        raise ScenarioError(
            f"proc_pid_rusage({pid}) failed with rc={rc} (no usable rusage flavor)"
        )
    user_ticks, system_ticks = struct.unpack_from(
        "<QQ", buffer.raw, _RUSAGE_TIMES_OFFSET
    )
    numer, denom = _mach_timebase()
    return (user_ticks + system_ticks) * numer / denom / 1e9


def _linux_cpu_seconds(pid: int) -> float:
    raw = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    # comm 可能含空格 / 括号 → 从**最后一个** ") " 之后切（字段 3 起）。
    fields = raw.rsplit(") ", 1)[-1].split()
    utime, stime = int(fields[11]), int(fields[12])
    ticks = float(os.sysconf("SC_CLK_TCK"))
    return (utime + stime) / ticks


def process_cpu_seconds(pid: int) -> float:
    """进程累计 CPU（user+sys，秒）的实时采样。

    linux 走 ``/proc/<pid>/stat``，darwin 走 ``libproc.proc_pid_rusage``；其它平台
    直接报错（宁可不测，也不产出假数字）。不用 ``ps -o time``（精度 1s），也不用
    ``resource.getrusage(RUSAGE_CHILDREN)``（只能事后读、且含启动 import 的 CPU）。
    """
    if sys.platform == "darwin":
        return _darwin_cpu_seconds(pid)
    if sys.platform.startswith("linux"):
        return _linux_cpu_seconds(pid)
    raise ScenarioError(f"no CPU sampling backend for platform {sys.platform!r}")


# ── 场景环境 ──────────────────────────────────────────────────


class ScenarioRun:
    """一个场景的隔离环境（fresh ``WING_HOME`` + 网关 + 假 Provider）。

    ``root`` 落在 ``<side.wing_home>/gateway/r<round>/<scenario>``；进入场景前整棵
    删除（重跑同一 round 也是白纸），场景成功即由套件收尾时删除，失败则保留现场。
    """

    def __init__(self, name: str, side: Side, round_no: int, out: SuiteOutput) -> None:
        self.name = name
        self.side = side
        self.round_no = round_no
        self.out = out
        self.model = f"{MODEL_PREFIX}/{name}"
        self.root = side.wing_home / "gateway" / f"r{round_no}" / name
        self.started = time.monotonic()
        self._probe: Probe | None = None

    async def __aenter__(self) -> ScenarioRun:
        shutil.rmtree(self.root, ignore_errors=True)
        self.root.mkdir(parents=True, exist_ok=True)
        self.started = time.monotonic()
        self._probe = await Probe.start(
            self.root,
            gateway_bin=self.side.gateway_bin,
            model=self.model,
            tools=TOOLS,
            system_prompt=SYSTEM_PROMPT,
            context_window_tokens=CONTEXT_WINDOW_TOKENS,
            keep_recent_tokens=KEEP_RECENT_TOKENS,
            health_timeout=HEALTH_TIMEOUT,
        )
        try:
            print(
                f"[gateway] scenario {self.name}: gateway up "
                f"(pid {self.pid}, {self.root})",
                flush=True,
            )
        except BaseException:
            # 没进 `async with` 就没有 `__aexit__`：已起的网关进程必须在这里收掉。
            await self._stop_probe()
            raise
        return self

    async def __aexit__(self, *_: object) -> None:
        await self._stop_probe()
        elapsed = time.monotonic() - self.started
        self.out.setting(f"{self.name}.duration_s", round(elapsed, 3))
        print(f"[gateway] scenario {self.name}: teardown in {elapsed:.1f}s", flush=True)

    async def _stop_probe(self) -> None:
        """停掉并忘掉本场景的 probe（幂等：`__aenter__` 失败与 `__aexit__` 共用）。"""
        probe, self._probe = self._probe, None
        if probe is not None:
            await probe.stop()

    @property
    def probe(self) -> Probe:
        if self._probe is None:
            raise ScenarioError(f"scenario {self.name} is not running")
        return self._probe

    @property
    def pid(self) -> int:
        """网关子进程 pid（CPU 采样用）。"""
        process = self.probe.env.process
        if process is None:
            raise ScenarioError(f"scenario {self.name}: gateway process is gone")
        return int(process.pid)

    @property
    def env(self) -> ProbeEnv:
        return self.probe.env


def _rel(run: ScenarioRun, t: float) -> float:
    """绝对 ``time.monotonic()`` → 「相对 env 启动」秒（与 ``Event.at`` 同尺度）。"""
    return t - run.env.started_at


def _error_summary(event: Event) -> str:
    """``error`` 事件的一行摘要：优先 ``[status_code] message``，否则退回紧凑 JSON。

    ``event.data`` 含整个错误事件字典，直接塞进失败摘要是半截 JSON（400 字符截断会切在
    ``{"er…`` 处）。完整现场仍在 stdout 的 traceback 与 ab 的 ``failures[].log`` 里。
    """
    data = event.data if isinstance(event.data, Mapping) else {}
    message = data.get("message")
    if isinstance(message, str) and message.strip():
        text = message.strip().splitlines()[0]
        status = data.get("status_code")
        return f"[{status}] {text}" if isinstance(status, int) else text
    try:
        return json.dumps(dict(data), ensure_ascii=False)[:400]
    except (TypeError, ValueError):  # pragma: no cover - 帧里的 JSON 必可再序列化
        return str(data)[:400]


async def _finish_turn(
    session: Session, content: str, *, within: float, label: str
) -> Event:
    """发一条消息并等到本轮 ``turn_result``（``error`` 事件即抛，不等满超时）。"""
    await session.send(content)
    event = await session.watch.expect(["turn_result", "error"], within=within)
    if event.type != "turn_result":
        raise ScenarioError(
            f"{label}: turn ended with an error event: {_error_summary(event)}"
        )
    return event


async def _record_gateway_identity(run: ScenarioRun, out: SuiteOutput) -> None:
    """把网关版本 / commit 记进 meta.config（产物溯源用；只记一次）。"""
    if "gateway.version" in out.config:
        return
    health = await run.probe.driver_required.http.health()
    if isinstance(health, Mapping):
        out.setting("gateway.version", health.get("version"))
        out.setting("gateway.commit", health.get("commit"))
        out.setting("gateway_bin", str(run.side.gateway_bin))


def _body_chars(body: Mapping[str, Any]) -> int:
    """请求体里 ``messages[].content`` 的字符总数（大 history 规模的证据）。"""
    messages = body.get("messages")
    if not isinstance(messages, list):
        return 0
    total = 0
    for message in messages:
        if isinstance(message, Mapping) and isinstance(message.get("content"), str):
            total += len(str(message["content"]))
    return total


def _text_events_after(session: Session, index: int) -> list[Event]:
    """时间线上 ``index`` 之后的全部 ``text`` 事件（content delta 帧）。"""
    return [
        event
        for event in session.timeline.all()
        if event.index >= index and event.type == "text"
    ]


# ── 场景 1：fanout ────────────────────────────────────────────


async def scenario_fanout(
    run: ScenarioRun, settings: Settings, out: SuiteOutput
) -> None:
    n = settings.fanout_frames
    probe = run.probe
    await _record_gateway_identity(run, out)
    probe.register(
        run.model,
        Script(
            Turn.of(text=WARMUP_TEXT),
            Turn.of(text="x" * n, chunk=FANOUT_CHUNK_CHARS),
        ),
    )
    session = await probe.session()
    await _finish_turn(session, "warm up", within=TURN_WITHIN, label="fanout warmup")
    await asyncio.sleep(SETTLE_S)

    cursor = len(session.timeline)
    frames_before = probe.driver_required.websocket.frames_received
    cpu_before = process_cpu_seconds(run.pid)
    t0 = time.monotonic()
    await session.send("stream")
    event = await session.watch.expect(["turn_result", "error"], within=FANOUT_WITHIN)
    cpu_after = process_cpu_seconds(run.pid)
    if event.type != "turn_result":
        raise ScenarioError(
            f"fanout: turn ended with an error event: {_error_summary(event)}"
        )

    frames = _text_events_after(session, cursor)
    frames_after = probe.driver_required.websocket.frames_received
    if len(frames) != n:
        out.note(
            f"fanout: provider 发出 {n} 帧，客户端收到 {len(frames)} 个 content 帧"
            "（帧数对不上，指标按实际帧数计算）"
        )
    if len(frames) < 2:
        raise ScenarioError(f"fanout: only {len(frames)} content frame(s) observed")

    complete_ms = (frames[-1].at - _rel(run, t0)) * 1000.0
    gaps_us = [
        (later.at - earlier.at) * 1e6 for earlier, later in zip(frames, frames[1:])
    ]
    cpu_ms = (cpu_after - cpu_before) * 1000.0
    per_1k = cpu_ms * (1000.0 / len(frames))
    if cpu_ms < 0:
        out.note(f"fanout: CPU 增量为负（{cpu_ms:.3f}ms）——采样异常")
    if event.at < frames[-1].at:
        out.note("fanout: turn_result 早于最后一个 content 帧（时间线异常）")

    out.metric("gateway.fanout.complete_ms", complete_ms)
    out.metric("gateway.fanout.gap_p99_us", percentile(gaps_us, 99), *gaps_us)
    out.metric("gateway.fanout.cpu_ms_per_1k", per_1k)

    out.setting("fanout.frames", n)
    out.setting("fanout.chunk_chars", FANOUT_CHUNK_CHARS)
    out.setting("fanout.warmup_turns", 1)
    out.setting("fanout.observed_frames", len(frames))
    out.setting("fanout.gap_p50_us", round(percentile(gaps_us, 50), 3))
    out.setting("fanout.gap_p90_us", round(percentile(gaps_us, 90), 3))
    out.setting("fanout.gap_max_us", round(max(gaps_us), 3))
    out.setting("fanout.client_frames", frames_after - frames_before)
    out.setting("fanout.turn_result_ms", round((event.at - _rel(run, t0)) * 1000, 3))
    out.setting("fanout.cpu_ms_total", round(cpu_ms, 3))
    out.setting("fanout.gap_clock", "client read-loop arrival (time.monotonic)")
    print(
        f"[gateway] fanout: frames={len(frames)} complete={complete_ms:.1f}ms "
        f"gap_p99={percentile(gaps_us, 99):.1f}us cpu/1k={per_1k:.2f}ms",
        flush=True,
    )


# ── 场景 2：turns ─────────────────────────────────────────────


async def scenario_turns(
    run: ScenarioRun, settings: Settings, out: SuiteOutput
) -> None:
    k = settings.mini_turns
    probe = run.probe
    probe.register(
        run.model,
        Script(Turn.of(text=WARMUP_TEXT), *[Turn.of(text="ok") for _ in range(k)]),
    )
    session = await probe.session()
    await _finish_turn(session, "warm up", within=TURN_WITHIN, label="turns warmup")
    await asyncio.sleep(SETTLE_S)

    samples: list[float] = []
    for index in range(k):
        t0 = time.monotonic()
        event = await _finish_turn(
            session, f"turn {index}", within=TURN_WITHIN, label=f"turns #{index}"
        )
        samples.append((event.at - _rel(run, t0)) * 1000.0)
    if len(samples) != k:
        out.note(f"turns: 只有 {len(samples)} 个轮次样本（期望 {k}）")

    out.metric("gateway.turn.p50_ms", percentile(samples, 50), *samples)
    out.metric("gateway.turn.p99_ms", percentile(samples, 99))
    out.setting("turns.count", k)
    out.setting("turns.warmup_turns", 1)
    out.setting("turns.samples", len(samples))
    out.setting("turns.min_ms", round(min(samples), 3))
    out.setting("turns.max_ms", round(max(samples), 3))
    out.setting("turns.percentile", "nearest-rank")
    print(
        f"[gateway] turns: k={k} p50={percentile(samples, 50):.1f}ms "
        f"p99={percentile(samples, 99):.1f}ms",
        flush=True,
    )


# ── 场景 3：context ───────────────────────────────────────────


async def scenario_context(
    run: ScenarioRun, settings: Settings, out: SuiteOutput
) -> None:
    probe = run.probe
    probe.register(
        run.model,
        Script(
            Turn.of(text=WARMUP_TEXT),
            *[
                Turn.of(text=settings.history_reply(index))
                for index in range(settings.history_turns)
            ],
            *[Turn.of(text="probe reply") for _ in range(settings.probe_prompts)],
        ),
    )
    session = await probe.session()
    await _finish_turn(session, "warm up", within=TURN_WITHIN, label="context warmup")
    for index in range(settings.history_turns):
        await _finish_turn(
            session,
            f"step {index}",
            within=TURN_WITHIN,
            label=f"context build #{index}",
        )
    await asyncio.sleep(SETTLE_S)

    requests = probe.requests.all()
    if not requests:
        raise ScenarioError("context: 假 Provider 没有收到任何请求")
    history_chars = _body_chars(requests[-1].body)
    out.setting("context.history_turns", settings.history_turns)
    out.setting("context.history_reply_chars", settings.history_reply_chars)
    out.setting("context.history_chars", history_chars)
    out.setting("context.probe_prompts", settings.probe_prompts)

    build_ms: list[float] = []
    probe_chars: list[int] = []
    for index in range(settings.probe_prompts):
        before = len(probe.requests.all())
        t0 = time.monotonic()
        await session.send(f"probe {index}")
        event = await session.watch.expect(["turn_result", "error"], within=TURN_WITHIN)
        if event.type != "turn_result":
            raise ScenarioError(
                f"context: probe #{index} ended with an error event: "
                f"{_error_summary(event)}"
            )
        fresh = probe.requests.all()[before:]
        if len(fresh) != 1:
            out.note(
                f"context: probe #{index} 触发 {len(fresh)} 次 provider 请求（期望 1）"
            )
        matched = next((entry for entry in fresh if entry.at >= t0), None)
        if matched is None:
            out.note(f"context: probe #{index} 没有晚于 send 的 provider 请求")
            continue
        build_ms.append((matched.at - t0) * 1000.0)
        probe_chars.append(_body_chars(matched.body))
    if len(build_ms) != settings.probe_prompts:
        out.note(
            f"context: 只有 {len(build_ms)} 个 build 样本（期望 {settings.probe_prompts}）"
        )
    if not build_ms:
        raise ScenarioError("context: 没有可用的 build 样本")

    out.metric("gateway.context.build_p50_ms", percentile(build_ms, 50), *build_ms)
    out.metric("gateway.context.build_p99_ms", percentile(build_ms, 99))
    out.setting("context.build_samples", len(build_ms))
    out.setting("context.build_min_ms", round(min(build_ms), 3))
    out.setting("context.build_max_ms", round(max(build_ms), 3))
    out.setting("context.probe_chars", probe_chars)
    out.setting("context.clock", "same-process time.monotonic (send vs provider at)")
    print(
        f"[gateway] context: history={history_chars}chars "
        f"build_p50={percentile(build_ms, 50):.2f}ms "
        f"build_p99={percentile(build_ms, 99):.2f}ms",
        flush=True,
    )


# ── 场景 4：resume ────────────────────────────────────────────


async def scenario_resume(
    run: ScenarioRun, settings: Settings, out: SuiteOutput
) -> None:
    probe = run.probe
    probe.register(
        run.model,
        Script(
            Turn.of(text=WARMUP_TEXT),
            *[
                Turn.of(text=settings.history_reply(index))
                for index in range(settings.history_turns)
            ],
        ),
    )
    session = await probe.session()
    await _finish_turn(session, "warm up", within=TURN_WITHIN, label="resume warmup")
    for index in range(settings.history_turns):
        await _finish_turn(
            session,
            f"step {index}",
            within=TURN_WITHIN,
            label=f"resume build #{index}",
        )
    session_id = session.session_id
    requests = probe.requests.all()
    history_chars = _body_chars(requests[-1].body) if requests else 0

    sync_ms: list[float] = []
    hydrate_ms: list[float] = []
    message_counts: list[int] = []
    for cycle in range(settings.resume_cycles):
        await probe.restart_gateway()
        driver = probe.driver_required
        started = time.monotonic()
        await driver.http.resume_session(session_id)
        hydrate_ms.append((time.monotonic() - started) * 1000.0)
        # 计时窗口只覆盖「subscribe → sync_session 重放完成」：水合（resume）在窗口之外
        # 单列进 meta.config（design D2/D11）。
        handle = await driver.attach(session_id, subscribe=False)
        client_id = driver.client_id
        t0 = time.monotonic()
        await driver.http.subscribe(session_id, client_id)
        event = await handle.watch.expect("sync_session", within=SYNC_WITHIN)
        sync_ms.append((run.env.started_at + event.at - t0) * 1000.0)
        messages = event.data.get("messages")
        message_counts.append(len(messages) if isinstance(messages, list) else -1)
        print(
            f"[gateway] resume: cycle {cycle + 1}/{settings.resume_cycles} "
            f"sync={sync_ms[-1]:.1f}ms messages={message_counts[-1]}",
            flush=True,
        )

    if not sync_ms:
        raise ScenarioError("resume: 没有可用的 sync 样本")
    if len(sync_ms) != settings.resume_cycles:
        out.note(
            f"resume: 只有 {len(sync_ms)} 个 sync 样本（期望 {settings.resume_cycles}）"
        )
    # 重放完整性：warmup + 每个构造轮次各一对 user/assistant ⇒ 2×(history_turns+1)。
    # 计数已经写进 meta.config.resume.messages，这里把"对不上"的事实也写进 notes
    # （部分重放 / 空重放都要能被报告看见）。
    expected_messages = 2 * (settings.history_turns + 1)
    if message_counts and min(message_counts) != expected_messages:
        out.note(
            f"resume: sync_session 重放 {message_counts} 条消息，"
            f"期望 {expected_messages}（= 2×({settings.history_turns}+1)）"
        )

    out.metric("gateway.resume.sync_ms", median(sync_ms), *sync_ms)
    out.setting("resume.cycles", settings.resume_cycles)
    out.setting("resume.history_turns", settings.history_turns)
    out.setting("resume.history_chars", history_chars)
    out.setting("resume.hydrate_ms", [round(value, 3) for value in hydrate_ms])
    out.setting("resume.sync_samples_ms", [round(value, 3) for value in sync_ms])
    out.setting("resume.messages", message_counts)
    out.setting("resume.session_id", session_id)
    out.setting("resume.window", "subscribe call → sync_session arrival")
    print(
        f"[gateway] resume: sync_ms={median(sync_ms):.1f}ms "
        f"hydrate_ms={[round(v, 1) for v in hydrate_ms]}",
        flush=True,
    )


# ── 编排 ──────────────────────────────────────────────────────

Scenario = Callable[[ScenarioRun, Settings, SuiteOutput], Awaitable[None]]

SCENARIOS: tuple[tuple[str, Scenario], ...] = (
    ("fanout", scenario_fanout),
    ("turns", scenario_turns),
    ("context", scenario_context),
    ("resume", scenario_resume),
)


async def run_suite(
    side: Side, settings: Settings, round_no: int, out: SuiteOutput
) -> list[str]:
    """顺序跑四个场景；返回失败摘要列表（空 = 全部成功）。

    每个场景自带隔离环境（`async with` 收尾）——场景失败只终止自己，其余继续；
    失败摘要进 `failures[]`（ab 的 `failures[]` 与退出码依据）。
    """
    failures: list[str] = []
    for name, scenario in SCENARIOS:
        try:
            async with ScenarioRun(name, side, round_no, out) as run:
                await scenario(run, settings, out)
        except Exception as exc:  # noqa: BLE001 - 场景失败即套件失败（显式记录）
            failures.append(
                f"{name}: {type(exc).__name__}: {exc}".splitlines()[0][:400]
            )
            print(
                f"[gateway] scenario {name} FAILED\n{traceback.format_exc()}",
                flush=True,
            )
    return failures


def record_settings(out: SuiteOutput, side: Side, settings: Settings) -> None:
    """把档位与 side 身份写进 meta.config（人工判读指标量级时必需）。"""
    out.setting("settings.fanout_frames", settings.fanout_frames)
    out.setting("settings.mini_turns", settings.mini_turns)
    out.setting("settings.history_turns", settings.history_turns)
    out.setting("settings.history_reply_chars", settings.history_reply_chars)
    out.setting("settings.probe_prompts", settings.probe_prompts)
    out.setting("settings.resume_cycles", settings.resume_cycles)
    out.setting("side.name", side.name)
    out.setting("side.worktree", str(side.worktree))
    out.setting("side.wing_home", str(side.wing_home))
    # 机器状态：本套件对同机负载敏感（CI runner 与开发机都可能被并行任务占用），
    # 判读指标时缺了它就只能靠猜。
    try:
        out.setting("machine.loadavg", [round(value, 2) for value in os.getloadavg()])
    except OSError:  # pragma: no cover - 无负载信息（少数平台）
        pass
    out.setting("machine.cpu_count", os.cpu_count())
    out.setting("machine.platform", sys.platform)


# ── CLI ───────────────────────────────────────────────────────


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="suite_gateway.py",
        description=(
            "perf-ci gateway suite: socket-level measurements of a real "
            "wing-gateway driven by a fake provider (see 05 design.md)"
        ),
    )
    parser.add_argument(
        "--side", type=Path, required=True, help="side descriptor JSON (contract §1)"
    )
    parser.add_argument(
        "--out", type=Path, required=True, help="suite output JSON (contract §3)"
    )
    parser.add_argument(
        "--round", type=int, default=1, help="round number stamped into the output"
    )
    parser.add_argument(
        "--quick", action="store_true", help="CI budget: smaller N / K / history"
    )
    return parser.parse_args(argv)


def validate_side(side: Side) -> str | None:
    """side 描述符的可用性检查（返回错误文案；None = 通过）。"""
    if not side.wing_home.is_absolute():
        # 契约 §1 要求绝对路径；相对路径会把 scratch 写进调用方 cwd（畸形 side 里
        # `null` 会被 `Side.load` 强转成 `Path("None")`）。
        return (
            f"side {side.name}: wing_home must be an absolute path (contract §1), "
            f"got {side.wing_home}"
        )
    if not side.gateway_bin.is_file():
        return (
            f"gateway binary not found: {side.gateway_bin} "
            "(run ab.py with --prepare: `uv sync --frozen`)"
        )
    if not side.python.is_file():
        return (
            f"side python not found: {side.python} "
            "(run ab.py with --prepare: `uv sync --frozen`)"
        )
    return None


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    settings = settings_for(quick=args.quick)
    try:
        side = Side.load(args.side)
    except (ValueError, RuntimeError) as exc:
        print(f"[gateway] error: {exc}", flush=True)
        return 2
    problem = validate_side(side)
    if problem:
        print(f"[gateway] error: {problem}", flush=True)
        return 2

    out = SuiteOutput("gateway", side.name, args.round, quick=args.quick)
    record_settings(out, side, settings)
    print(
        f"[gateway] side={side.name} round={args.round} quick={args.quick} "
        f"gateway={side.gateway_bin}",
        flush=True,
    )
    failures = asyncio.run(run_suite(side, settings, args.round, out))
    missing = [metric for metric in EXPECTED_METRICS if metric not in out.metrics]
    if missing:
        message = f"metrics missing after all scenarios: {', '.join(missing)}"
        out.note(message)
        if not failures:
            failures.append(message)

    gateway_root = side.wing_home / "gateway"
    if failures:
        # 现场（网关日志 / 会话落盘）留着给人工取证——路径写进 notes，别让人只能靠猜。
        out.note(f"失败现场保留在 {gateway_root}（成功时套件会自动清理）")
        out.write(args.out, ok=False, error=" | ".join(failures))
        print(f"[gateway] FAILED: {' | '.join(failures)}", flush=True)
        return 3
    out.write(args.out)
    shutil.rmtree(gateway_root, ignore_errors=True)
    print(f"[gateway] ok → {args.out}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
