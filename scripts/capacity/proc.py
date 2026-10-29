#!/usr/bin/env python3
"""进程级采样：累计 CPU 秒 / RSS / 打开的 fd 数 + fd 软限抬升。

CPU 采样的平台分支照搬 ``scripts/perf/suite_gateway.py`` 的
``_darwin_cpu_seconds`` / ``_linux_cpu_seconds`` / ``process_cpu_seconds``
（macOS 走 ``libproc.proc_pid_rusage`` + mach timebase，Linux 走
``/proc/<pid>/stat``；不用 ``ps -o time``——精度 1 s，也不用
``getrusage(RUSAGE_CHILDREN)``——只能事后读且含解释器启动的 CPU）。

**为什么不共享**：那份实现住在一个 perf 套件脚本里（不是共用模块），跨工具 import
会把整条 perf 测量链（side 描述符 / 判定 / 子进程编排）拖进手测工具；而容量测量还
多要 RSS / fd / rlimit 三个读取口。**origin 是 perf 那一份**（更早、被 CI 覆盖、与
``docs/dev/perf-testing.md`` 对齐）：改任何一边都要同步另一边，合并时以那边为准。

只用标准库；不引入 psutil。
"""

from __future__ import annotations

import ctypes
import ctypes.util
import os
import resource
import struct
import subprocess
import sys
from pathlib import Path
from typing import Any

#: RSS / fd 采样的子进程超时（秒）——进程卡死时不把测量挂在这里。
PS_TIMEOUT = 5.0

#: fd 软限的每个会话预算：一个 WS 连接 + 一条上游 HTTP 连接，再留一倍余量。
NOFILE_PER_SESSION = 4.0

#: fd 软限的固定余量：解释器 / 网关自身的日志、SQLite、监听、临时文件。
NOFILE_HEADROOM = 256


class ProcError(RuntimeError):
    """进程采样失败（进程已退出 / 平台不支持 / 权限不足）。"""


# ── CPU（累计秒）────────────────────────────────────────────────

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
            raise ProcError(
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
        lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
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
        raise ProcError(
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
    """进程累计 CPU（user+sys，秒）。

    采样口径是「窗口首尾两次累计值之差 / 窗口秒数」——不做周期采样：周期读会把
    ``ps`` / ``lsof`` 自己的开销算进被测进程旁边。

    只用于**别的**进程（被测网关）。自己的进程用 :func:`self_cpu_seconds`：分片与
    Provider 在窗口结束时自己记账，避免与「读完就退出」的子进程收尸竞态。
    """
    if sys.platform == "darwin":
        return _darwin_cpu_seconds(pid)
    if sys.platform.startswith("linux"):
        return _linux_cpu_seconds(pid)
    raise ProcError(f"no CPU sampling backend for platform {sys.platform!r}")


def self_cpu_seconds() -> float:
    """本进程累计 CPU（user+sys，秒）——``getrusage(RUSAGE_SELF)``，全平台一致。"""
    usage = resource.getrusage(resource.RUSAGE_SELF)
    return float(usage.ru_utime + usage.ru_stime)


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
