"""场景 fixture（tasks 6.1）——每个场景一个独立的 probe 环境。

本文件是 ``scenarios/`` 唯一的公共 fixture 定义处（design D11）：场景文件只
import ``wing_probe`` 与 pytest。

``probe`` fixture（function 级）提供：

1. **自举**：``Probe.start(tmp_path / "probe")`` —— 临时 ``WING_HOME`` + 独立
   网关子进程 + 进程内假 Provider（design D7/D10，场景之间零共享）；
2. **teardown 自动不变量**：对场景创建／挂载的**全部** session（含 fork 出来的
   子会话）运行三条内置不变量（链拓扑 / tool 配对 / transient 不落盘）。失败即
   抛 :class:`wing_probe.ProbeInvariantError`（场景失败，报告标注来源
   ``built-in invariants``、session id 与不变量名）；
3. **失败现场转储**：场景失败（或 ``PROBE_DUMP=always``）时把时间线全帧、原始帧、
   HTTP 留档、假 Provider 请求留档、各 session 落盘拷贝与网关日志写进
   ``<env.root>/artifacts/``；转储路径进失败报告（``watch.dump_path`` 已指向该
   目录，expect 超时报告末行即引用它）与终端汇总。

``PROBE_DUMP`` 取值：

- ``on-fail``（默认）：场景失败或内置不变量失败时转储；
- ``always``：每个场景都转储（本地排查"绿场景长什么样"时用）；
- ``never``：从不自动转储（``probe.dump()`` 显式调用仍可用）。

逃生舱 ``probe.without_invariants(reason=...)`` 必须给理由：理由写进
``artifacts/dump.txt``，并在终端汇总里回显（spec「逃生舱必须说明理由」）。
"""

from __future__ import annotations

import os
from collections.abc import AsyncIterator, Iterator
from pathlib import Path
from typing import Any

import pytest
import pytest_asyncio

from wing_probe import Probe, ProbeInvariantError

#: 现场转储策略（``on-fail`` / ``always`` / ``never``）。
DUMP_MODE_ENV = "PROBE_DUMP"
DUMP_MODES = ("on-fail", "always", "never")

#: 终端汇总的数据源：nodeid → (转储路径, 逃生舱理由)。
_REPORTS: dict[str, tuple[Path | None, str | None]] = {}


def dump_mode() -> str:
    """解析 ``PROBE_DUMP``（非法值直接报错，不静默退化）。"""
    raw = os.environ.get(DUMP_MODE_ENV, "on-fail").strip().lower()
    if raw not in DUMP_MODES:
        raise pytest.UsageError(
            f"{DUMP_MODE_ENV}={raw!r} is not a valid dump mode; "
            f"use one of {', '.join(DUMP_MODES)}"
        )
    return raw


def call_phase_failed(item: pytest.Item) -> bool:
    """call 阶段是否失败（``pytest_runtest_makereport`` 挂的 rep_call）。"""
    report = getattr(item, "rep_call", None)
    return bool(report is not None and report.failed)


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(
    item: pytest.Item, call: pytest.CallInfo[Any]
) -> Iterator[Any]:
    """记录各阶段报告（fixture teardown 需要知道 call 阶段是否失败）。"""
    report = yield
    if report is not None:
        setattr(item, f"rep_{report.when}", report)
    return report


def pytest_terminal_summary(terminalreporter: Any) -> None:
    """失败报告末尾列出各场景的现场转储路径（design D8：CI 收 artifacts）。"""
    entries = {
        nodeid: item
        for nodeid, item in _REPORTS.items()
        if item[0] is not None or item[1] is not None
    }
    if not entries:
        return
    terminalreporter.write_sep("=", "wing-probe artifacts")
    for nodeid, (path, reason) in sorted(entries.items()):
        note = f"  (invariants disabled: {reason})" if reason else ""
        terminalreporter.write_line(f"  {nodeid} → {path}{note}")


@pytest_asyncio.fixture
async def probe(request: pytest.FixtureRequest, tmp_path: Path) -> AsyncIterator[Probe]:
    """一个场景的 probe 门面（自举 + teardown 不变量 + 失败转储）。"""
    mode = dump_mode()
    instance = await Probe.start(tmp_path / "probe")
    try:
        yield instance
    finally:
        nodeid = request.node.nodeid
        problems: list[str] = []
        if instance.invariants_enabled:
            problems = instance.check_invariants()
        failed = call_phase_failed(request.node) or bool(problems)
        dump_path: Path | None = None
        if mode == "always" or (mode == "on-fail" and failed):
            try:
                dump_path = await instance.dump()
            except Exception as exc:  # 转储失败不得掩盖真正的失败原因
                problems.append(f"[dump] failed to write artifacts: {exc!r}")
        report = instance.invariant_report(problems) if problems else None
        await instance.stop()
        _REPORTS[nodeid] = (dump_path, instance.invariants_reason)
        if report is not None:
            raise ProbeInvariantError(report)
