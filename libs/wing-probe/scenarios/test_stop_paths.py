"""两条停机路径的**可观察差别**（``stop()`` 快 vs ``shutdown()`` 优雅）。

fixture teardown 用的是快路径（``Probe.stop`` → SIGKILL，不跑 lifespan），理由见
``ProbeEnv.stop``：teardown 之后没有任何断言，而优雅停机固定要 0.3s。代价是
「停机时发生的事」默认不会被验证——本文件把两条路径的差别钉在**外部可观察量**上，
而不是钉在 probe 自己的意图上：

- 快路径：退出码是 SIGKILL（``-9``），且 ``core/wing.db-wal`` 留在盘上——lifespan 收尾的
  ``close_database()`` 根本没跑（SQLite 没机会 checkpoint + 删除 WAL）；
- 优雅路径：退出码是 SIGTERM（uvicorn 捕获取消后按惯例 re-raise 捕获的信号，
  ``capture_signals``），``-wal`` 被收掉——``close_database()`` 真的跑了。

这也是「需要观察停机语义时怎么写」的活样例：在**场景体里**显式 ``probe.shutdown()``，
不要指望 teardown。
"""

from __future__ import annotations

import signal
from pathlib import Path

import pytest

from wing_probe import Probe, ProbeEnv

#: 网关的退出码（子进程视角）：快路径吃 SIGKILL，优雅路径吃 SIGTERM（见文件头）。
#: 优雅路径的 ``-SIGTERM`` 是 uvicorn 的行为而非产品契约——它捕获取消后会把捕获到的
#: 信号 re-raise（``Server.capture_signals``），所以"进程自己退干净"在退出码上仍显示
#: 为被 SIGTERM 终止。主断言是 WAL 与兜底日志标记，退出码只是佐证。
SIGKILL_EXIT = -signal.SIGKILL
SIGTERM_EXIT = -signal.SIGTERM

#: auth 场景的两把 key：`/api/shutdown` 不在 tool_runtime 的 allowlist 里，
#: 优雅收尾必须挑到 admin 那一把（`ProbeEnv.shutdown_api_key`）。
ADMIN_KEY = "probe-stop-admin"
TOOL_RUNTIME_KEY = "probe-stop-tool"


def _wal(env: ProbeEnv) -> Path:
    """进程级 SQLite 的 WAL 边车文件（``close_database()`` 收掉它）。"""
    return env.wing_home / "core" / "wing.db-wal"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_fast_stop_skips_the_lifespan_teardown(probe: Probe) -> None:
    """``stop()``：SIGKILL，lifespan 一行不跑（WAL 留在盘上）。"""
    # 前提：网关启动期真的建过库并写下 WAL——否则下面的断言是空转。
    assert _wal(probe.env).exists(), "gateway never opened wing.db in WAL mode"
    process = probe.env.process
    assert process is not None and process.poll() is None

    await probe.stop()

    assert process.returncode == SIGKILL_EXIT, process.returncode
    assert _wal(probe.env).exists(), (
        "fast stop removed the WAL — lifespan teardown ran, so stop() is not fast"
    )
    log = probe.env.log_tail(max_lines=200)
    assert "--- probe: kill -9 gateway ---" in log, log
    assert "--- probe: terminate() ---" not in log, log


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_shutdown_runs_the_lifespan_teardown(probe: Probe) -> None:
    """``shutdown()``：网关自己走 lifespan 收尾，``close_database()`` 收掉 WAL。"""
    assert _wal(probe.env).exists(), "gateway never opened wing.db in WAL mode"
    process = probe.env.process
    assert process is not None and process.poll() is None

    await probe.shutdown()

    assert not _wal(probe.env).exists(), (
        "shutdown() left the WAL behind — close_database() never ran"
    )
    assert process.returncode == SIGTERM_EXIT, process.returncode
    log = probe.env.log_tail(max_lines=200)
    # 自己退干净的证据：没有落到 terminate()/kill() 兜底。
    assert "--- probe: terminate() ---" not in log, log
    assert "--- probe: kill() ---" not in log, log


@pytest.mark.probe_env(
    auth={
        "enabled": True,
        "keys": [{"key": ADMIN_KEY}, {"key": TOOL_RUNTIME_KEY, "role": "tool_runtime"}],
    },
    api_key=ADMIN_KEY,
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_shutdown_uses_an_admin_key_when_auth_is_on(probe: Probe) -> None:
    """auth 打开时优雅收尾要挑对 key（``/api/shutdown`` 不在 tool_runtime 的 allowlist 里）。

    挑错 key 的后果是 401 → 收尾退到 ``terminate`` 兜底：退出码同样是 SIGTERM，
    所以**不能**只看退出码——这里钉的是「没有落到兜底」（日志里没有 terminate 标记）。
    """
    assert probe.env.shutdown_api_key == ADMIN_KEY
    process = probe.env.process
    assert process is not None

    await probe.shutdown()

    log = probe.env.log_tail(max_lines=200)
    assert "--- probe: terminate() ---" not in log, log
    assert process.returncode == SIGTERM_EXIT, process.returncode


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_shutdown_is_a_noop_after_stop(probe: Probe) -> None:
    """单向闸门：``stop()`` 之后 ``shutdown()`` 不再碰已经停掉的环境。

    fixture teardown 随后还会调一次 ``stop()``——三条路径共用同一个闸门，重复调用是
    no-op（不会去连一个已死的网关，也不会重复收假 Provider）。
    """
    process = probe.env.process
    assert process is not None

    await probe.stop()
    assert process.returncode == SIGKILL_EXIT
    await probe.shutdown()
    await probe.stop()

    assert probe.env.provider.started is False
    assert probe.env.process is None
