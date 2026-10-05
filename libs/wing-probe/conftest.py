"""probe 的唯一公共 fixture 定义处 —— **位置在 args 那一层，不是 scenarios/ 里**。

为什么住这一层：pytest 的 conftest 加载只从某个路径**向上**走
（`Config._importconftest`：`for parent in reversed((directory, *directory.parents))`），
而从不下潜。执行测试的进程在收集时会把每一层的 conftest 都加载上，但 **xdist 的
controller 不做收集** —— 它只为 `args` 调这个函数。所以 `pytest libs/wing-probe/`
（`make test-probe` 的原样调用）时，controller 只加载本文件，**不会**加载
`scenarios/conftest.py`。

后果不是"少打印一节"那么轻：任何必须在 controller 上跑的钩子（artifacts 台账的
合并与终端汇总）若定义在 `scenarios/` 里，并行时就成了死代码——失败现场的转储路径
静默消失，而 CI 上那是唯一的现场。搬到这里之后，`pytest libs/wing-probe/`、
`pytest libs/wing-probe/scenarios/`、单文件参数三种形态 controller 都能加载到
（本层是它们的 args 或 args 的祖先）。`tests/test_conftest_layout.py` 守这条约定。

本文件是 ``scenarios/`` 唯一的公共 fixture 定义处（design D11：场景文件只
import ``wing_probe`` 与 pytest）。

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

**并行**：场景之间零共享（各自 tmp + 各自网关子进程 + 各自假 Provider），所以
可以按核数并行跑（``make test-probe`` 的 ``-n``）。并行下 fixture 跑在 worker
进程里，artifacts 台账经 ``pytest_sessionfinish`` / ``pytest_testnodedown``
汇回 controller——终端汇总与失败报告里的转储路径与串行时一致（不因并行而丢）。
台账与汇总钩子住在本文件，正是为了保证 controller 加载得到（见开头）。
"""

from __future__ import annotations

import importlib.util
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

#: artifacts 台账从 worker 交回 controller 的键（见 ``pytest_sessionfinish``）。
WORKER_REPORTS_KEY = "probe_artifacts"


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


def pytest_sessionfinish(session: pytest.Session) -> None:
    """worker 收尾：把本进程的 artifacts 台账交回 controller。

    并行下 fixture 跑在 worker 进程里、终端汇总只在 controller 打印——不显式
    回收，失败现场的转储路径就会**静默消失**（看起来只是"今天没转储"，而 CI 上
    artifacts 是唯一的现场）。串行运行没有 ``workeroutput``，这里是 no-op。

    只声明用得上的参数（pluggy 允许 hook 实现取 hookspec 的子集）：xdist 在
    ``pytest_sessionfinish`` 的 hookwrapper 里发回 ``workeroutput``，本实现跑在
    ``yield`` 之内，因此台账一定先落进那个 dict、再被送回 controller。
    """
    workeroutput = getattr(session.config, "workeroutput", None)
    if not isinstance(workeroutput, dict):
        return
    workeroutput[WORKER_REPORTS_KEY] = {
        nodeid: [str(path) if path is not None else None, reason]
        for nodeid, (path, reason) in _REPORTS.items()
    }


# xdist 的钩子**只在 xdist 可导入时**定义：conftest 里的未知钩子名是
# INTERNALERROR（不是警告），没装 xdist 的环境不该因为这段可选集成整体跑不起来。
if importlib.util.find_spec("xdist") is not None:

    def pytest_testnodedown(node: Any, error: Any) -> None:
        """controller 收尾：合并各 worker 交回的 artifacts 台账。"""
        payload = getattr(node, "workeroutput", {}).get(WORKER_REPORTS_KEY)
        if not isinstance(payload, dict):
            return
        for nodeid, (path, reason) in payload.items():
            _REPORTS[nodeid] = (Path(path) if path else None, reason)


@pytest_asyncio.fixture
async def probe(request: pytest.FixtureRequest, tmp_path: Path) -> AsyncIterator[Probe]:
    """一个场景的 probe 门面（自举 + teardown 不变量 + 失败转储）。

    **环境旋钮**：测试可打 ``@pytest.mark.probe_env(<kwargs>)`` 覆盖 ``ProbeEnv``
    的启动参数（kwargs 原样透传给 ``Probe.start``）——例如逐出场景用
    ``sessions={"eviction": {"idle_ttl_seconds": 1.0}}`` 把 TTL 压到秒级。
    缺省不打标记＝标准 probe 配置。
    """
    mode = dump_mode()
    marker = request.node.get_closest_marker("probe_env")
    env_kwargs = dict(marker.kwargs) if marker is not None else {}
    instance = await Probe.start(tmp_path / "probe", **env_kwargs)
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
