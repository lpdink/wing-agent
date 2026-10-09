"""``ProbeEnv.start()`` 的失败路径收尾 —— 已 spawn 的网关不许变成孤儿（AD17）。

背景：``test_model_id_cross_provider`` 的那条旧场景断言"坏配置 ⇒ 网关拒绝启动"，
setup mode 之后网关**不再拒绝启动**，于是 ``ProbeEnv.start`` 成功返回、env 被丢掉、
teardown 从不跑 —— 每跑一次漏一个网关子进程（审查 B2 / AD17 记录了 4~8 个孤儿）。

根因已在场景侧修掉（env 被 ``try/finally`` 持有），这里钉住**基建的第二道**：
``start()`` 中途失败（进程起了但健康检查不过）时，那个进程必须被收干净。
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from wing_probe.env import ProbeEnvError

#: 假网关：写完自己的 pid 就把自己替换成 ``sleep``（保持同一个 pid 且不监听端口）。
_FAKE_GATEWAY = '#!/bin/sh\necho $$ > "$PROBE_PID_FILE"\nexec sleep 300\n'


def _pid_alive(pid: int) -> bool:
    """进程是否还活着（``kill(pid, 0)``：ProcessLookupError = 已回收）。"""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:  # pragma: no cover - 他人的进程（不可能是这里）
        return True
    return True


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_start_failure_terminates_the_spawned_gateway(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``start()`` 失败（健康检查超时）⇒ 已起的子进程被 terminate + 收尸（不留孤儿）。"""
    fake_gateway = tmp_path / "fake-gateway"
    fake_gateway.write_text(_FAKE_GATEWAY, encoding="utf-8")
    fake_gateway.chmod(0o755)
    pid_file = tmp_path / "child.pid"

    # terminate 的等待窗口压到 0.2s：本用例只验"收干净"，不验真实关停的耐心。
    monkeypatch.setattr("wing_probe.env.DEFAULT_SHUTDOWN_WAIT", 0.2)

    from wing_probe.env import ProbeEnv

    with pytest.raises(ProbeEnvError):
        await ProbeEnv.start(
            tmp_path / "env",
            gateway_bin=fake_gateway,
            health_timeout=0.3,
            gateway_attempts=1,
            env_overrides={"PROBE_PID_FILE": str(pid_file)},
        )

    # 子进程确实起过（pid 文件是它自己写的）——否则这条用例什么都没验。
    pid = int(pid_file.read_text(encoding="utf-8").strip())
    assert not _pid_alive(pid), f"spawned gateway {pid} leaked after start() failure"


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_stop_is_safe_after_start_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """失败路径的 ``stop()``（``start()`` 内部已调过一次）幂等且不抛。"""
    fake_gateway = tmp_path / "fake-gateway"
    fake_gateway.write_text(_FAKE_GATEWAY, encoding="utf-8")
    fake_gateway.chmod(0o755)
    pid_file = tmp_path / "child.pid"
    monkeypatch.setattr("wing_probe.env.DEFAULT_SHUTDOWN_WAIT", 0.2)

    from wing_probe.env import ProbeEnv

    env = ProbeEnv(
        tmp_path / "env",
        gateway_bin=fake_gateway,
        health_timeout=0.3,
        gateway_attempts=1,
        env_overrides={"PROBE_PID_FILE": str(pid_file)},
    )
    with pytest.raises(ProbeEnvError):
        await env.start_gateway()

    await env.stop()  # 首次
    await env.stop()  # 幂等
    pid = int(pid_file.read_text(encoding="utf-8").strip())
    assert not _pid_alive(pid)
