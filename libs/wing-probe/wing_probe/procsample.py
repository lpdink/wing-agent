"""测量原语（纯标准库）：进程 CPU 采样 / nearest-rank 百分位 / 原子写 JSON。

两个手工测量工具都要这几样：``scripts/perf/`suite_gateway.py`` 与
``scripts/capacity/``（进程采样、延迟统计、产物落盘）。把它们放在这里而不是各留一份，
是因为 CPU 采样是**平台 FFI**（macOS 走 ``libproc.proc_pid_rusage`` + mach timebase、
Linux 走 ``/proc/<pid>/stat``）：抄错一处就是静默错数，而两份实现之间没有任何一致性
门禁（两个工具的 ``--selftest`` 都不在 CI 里）。

本模块只 import 标准库——probe 的 AST 门禁禁止 ``import wing``，因此这里也不做
``wing.common.fs`` 之类的事：``write_json`` 自己 tmp + rename。
"""

from __future__ import annotations

import ctypes
import ctypes.util
import json
import math
import os
import resource
import struct
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any


class CpuSampleError(RuntimeError):
    """读不到进程 CPU（进程已退出 / 平台没有后端）——不降级成假数字。"""


# ── 进程 CPU（累计秒）────────────────────────────────────────────

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
            raise CpuSampleError(
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
        raise CpuSampleError(
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

    Linux 走 ``/proc/<pid>/stat``，macOS 走 ``libproc.proc_pid_rusage``；其它平台直接
    报错（宁可不测，也不产出假数字）。不用 ``ps -o time``（精度 1 s），也不用
    ``resource.getrusage(RUSAGE_CHILDREN)``（只能事后读、且含解释器启动的 CPU）。
    """
    if sys.platform == "darwin":
        return _darwin_cpu_seconds(pid)
    if sys.platform.startswith("linux"):
        return _linux_cpu_seconds(pid)
    raise CpuSampleError(f"no CPU sampling backend for platform {sys.platform!r}")


def self_cpu_seconds() -> float:
    """本进程累计 CPU（user+sys，秒）——``getrusage(RUSAGE_SELF)``，全平台一致。

    分片与假 Provider 在窗口结束时自己记账，用它而不是 ``process_cpu_seconds(os.getpid())``：
    后者要读自己的 ``/proc`` / rusage 结构，与「读完就退出」的子进程状态竞态。
    """
    usage = resource.getrusage(resource.RUSAGE_SELF)
    return float(usage.ru_utime + usage.ru_stime)


# ── 百分位 ─────────────────────────────────────────────────────


def percentile(values: Sequence[float], pct: float) -> float:
    """nearest-rank 百分位（p99 = 排序后第 ``ceil(0.99·n)`` 个值，不插值）。

    空序列 → ``ValueError``（没有样本就没有百分位，不返回 0）。
    """
    ordered = sorted(values)
    if not ordered:
        raise ValueError("percentile() of an empty sequence")
    rank = max(1, math.ceil(pct / 100.0 * len(ordered)))
    return float(ordered[min(rank, len(ordered)) - 1])


# ── 落盘 ───────────────────────────────────────────────────────


def write_json(path: Path, payload: Any) -> None:
    """原子写 JSON（tmp + rename），并拒绝 NaN/Infinity（非标准 JSON）。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(payload, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


__all__ = [
    "CpuSampleError",
    "percentile",
    "process_cpu_seconds",
    "self_cpu_seconds",
    "write_json",
]
