#!/usr/bin/env python3
"""进程级采样：累计 CPU 秒 / RSS / 打开的 fd 数 + fd 软限抬升。

CPU 采样的平台分支（macOS ``libproc.proc_pid_rusage`` + mach timebase / Linux
``/proc/<pid>/stat``）住在 :mod:`wing_probe.procsample`——同一份实现也给
``scripts/perf/suite_gateway.py`` 用：那是平台 FFI，抄两份就会静默错数。
RSS / fd / rlimit 是本工具自己的读法（``ps`` / ``lsof`` / ``resource``）。

只用标准库；不引入 psutil。
"""

from __future__ import annotations

import os
import resource
import subprocess
import sys
from pathlib import Path
from typing import Any

from wing_probe import procsample

#: RSS / fd 采样的子进程超时（秒）——进程卡死时不把测量挂在这里。
PS_TIMEOUT = 5.0

#: fd 软限的每个会话预算：一个 WS 连接 + 一条上游 HTTP 连接，再留一倍余量。
NOFILE_PER_SESSION = 4.0

#: fd 软限的固定余量：解释器 / 网关自身的日志、SQLite、监听、临时文件。
NOFILE_HEADROOM = 256


class ProcError(RuntimeError):
    """进程采样失败（进程已退出 / 平台不支持 / 权限不足）。"""


# ── CPU（累计秒）────────────────────────────────────────────────
#
# 平台分支不在这里：见 ``wing_probe.procsample``（与 perf 套件同一份实现）。
process_cpu_seconds = procsample.process_cpu_seconds
self_cpu_seconds = procsample.self_cpu_seconds

# ── RSS ────────────────────────────────────────────────────────


def _ps_field(pid: int, field: str) -> float:
    """``ps -o <field>= -p <pid>`` 的单值（KB）。进程不在 → ProcError。"""
    try:
        out = subprocess.run(
            ["ps", "-o", f"{field}=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=PS_TIMEOUT,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise ProcError(f"ps failed for pid {pid}: {exc}") from exc
    text = out.stdout.strip()
    if out.returncode != 0 or not text:
        raise ProcError(
            f"ps {field} for pid {pid} returned {out.returncode}: "
            f"{out.stderr.strip() or out.stdout.strip() or '<empty>'}"
        )
    try:
        return float(text.split()[0])
    except ValueError as exc:  # pragma: no cover - ps 输出格式异常
        raise ProcError(f"ps {field} for pid {pid} is not numeric: {text!r}") from exc


def process_rss_mb(pid: int) -> float:
    """常驻内存（MB）。

    Linux 读 ``/proc/<pid>/statm``（免子进程）；macOS 用 ``ps -o rss=``（``lsof``
    之外没有更便宜且稳定的读取口——``proc_pidinfo`` 的结构体随内核版本漂移）。
    """
    if sys.platform.startswith("linux"):
        raw = Path(f"/proc/{pid}/statm").read_text(encoding="ascii").split()
        pages = float(raw[1])
        return pages * os.sysconf("SC_PAGE_SIZE") / (1024 * 1024)
    if sys.platform == "darwin":
        return _ps_field(pid, "rss") / 1024.0
    raise ProcError(f"no RSS sampling backend for platform {sys.platform!r}")


# ── fd 数 ──────────────────────────────────────────────────────


def process_fds(pid: int) -> int:
    """进程当前打开的 fd 数。

    Linux 直接数 ``/proc/<pid>/fd``；macOS 数 ``lsof -p <pid>`` 的条目（平台惯例，
    成本随 fd 数线性增长，因此采样间隔按秒计而不是按帧计）。
    """
    if sys.platform.startswith("linux"):
        try:
            return len(os.listdir(f"/proc/{pid}/fd"))
        except OSError as exc:
            raise ProcError(f"cannot list /proc/{pid}/fd: {exc}") from exc
    if sys.platform == "darwin":
        try:
            out = subprocess.run(
                ["lsof", "-p", str(pid)],
                capture_output=True,
                text=True,
                timeout=PS_TIMEOUT,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ProcError(f"lsof failed for pid {pid}: {exc}") from exc
        lines = [line for line in out.stdout.splitlines() if line.strip()]
        if not lines:
            raise ProcError(
                f"lsof -p {pid} listed nothing (exit {out.returncode}): "
                f"{out.stderr.strip() or '<empty>'}"
            )
        return len(lines) - 1  # 去掉表头
    raise ProcError(f"no fd sampling backend for platform {sys.platform!r}")


# ── fd 软限 ────────────────────────────────────────────────────


def nofile_target(sessions: int) -> int:
    """满足 N 个并发会话应达到的 fd 软限（见 :data:`NOFILE_PER_SESSION`）。"""
    return int(NOFILE_PER_SESSION * max(sessions, 0) + NOFILE_HEADROOM)


def raise_nofile(target: int) -> tuple[int, int]:
    """把 ``RLIMIT_NOFILE`` 软限抬到 ``target``（封顶硬限），返回生效的 ``(soft, hard)``。

    1000 会话（1000 条 WS + 1000 条上游连接）在默认软限 256 上会秒 ``EMFILE``，
    而 ``EMFILE`` 出现在窗口中途时产出的是一张「好看的空表」。因此调用方必须在
    **起任何子进程之前**抬限；抬不到目标即抛（子进程继承父进程的软限，晚抬没用）。
    """
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft >= target:
        return int(soft), int(hard)
    ceiling = target if hard == resource.RLIM_INFINITY else min(target, int(hard))
    try:
        resource.setrlimit(resource.RLIMIT_NOFILE, (ceiling, hard))
    except (ValueError, OSError) as exc:
        raise ProcError(
            f"cannot raise the open-file soft limit to {target} (hard limit "
            f"{'infinity' if hard == resource.RLIM_INFINITY else hard}): {exc}"
        ) from exc
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if int(soft) < target:
        raise ProcError(
            f"open-file soft limit is {soft} after the raise, below the {target} "
            f"needed for this ladder (hard limit "
            f"{'infinity' if hard == resource.RLIM_INFINITY else hard}); raise the "
            "hard limit (macOS: `sudo launchctl limit maxfiles 65536 200000`) or "
            "lower --levels"
        )
    return int(soft), int(hard)


# ── 主机信息 ───────────────────────────────────────────────────


def host_info() -> dict[str, Any]:
    """``run.json`` 的 ``meta.host``：核数 / 内存 / 平台 / 软限。

    内存走 ``SC_PHYS_PAGES``（Linux / macOS 通用），不用 ``sysctl`` / ``/proc/meminfo``。
    """
    soft, _ = resource.getrlimit(resource.RLIMIT_NOFILE)
    return {
        "cpus": os.cpu_count(),
        "mem_gb": round(
            os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE") / (1024**3), 1
        ),
        "platform": sys.platform,
        "ulimit_n": int(soft),
    }
