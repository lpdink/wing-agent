"""``ProbeEnv.start()`` 的失败路径收尾 —— 已 spawn 的网关不许变成孤儿（AD17）。

背景：``test_model_id_cross_provider`` 的那条旧场景断言"坏配置 ⇒ 网关拒绝启动"，
setup mode 之后网关**不再拒绝启动**，于是 ``ProbeEnv.start`` 成功返回、env 被丢掉、
teardown 从不跑 —— 每跑一次漏一个网关子进程（审查 B2 / AD17 记录了 4~8 个孤儿）。

根因已在场景侧修掉（env 被 ``try/finally`` 持有），这里钉住**基建的第二道**：
``start()`` 中途失败（进程起了但健康检查不过）时，那个进程必须被收干净。
"""

from __future__ import annotations

import asyncio
import os
import signal
import time
from pathlib import Path

import pytest

from wing_probe.env import ProbeEnvError

#: 假网关：写完自己的 pid 就把自己替换成 ``sleep``（保持同一个 pid 且不监听端口）。
_FAKE_GATEWAY = '#!/bin/sh\necho $$ > "$PROBE_PID_FILE"\nexec sleep 300\n'


def _read_pid(path: Path, *, timeout: float = 5.0) -> int:
    """等假网关把自己的 pid 写出来（fork 之后 shell 要几毫秒才轮到跑）。

    等不到 = 用例自身失效（没起过进程就谈不上"收干净"）。**不能**把"起没起过"
    的判定建在立刻读文件上：外层兜底路径没有任何等待，读得太早是假红（review S2
    的用例必须能区分"泄漏"与"还没写"）。
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            text = path.read_text(encoding="utf-8").strip()
            if text:
                return int(text)
        time.sleep(0.02)
    raise AssertionError(f"fake gateway never wrote its pid to {path}")


def _reap(pid: int) -> None:
    """best-effort 收尸：失败路径上被**检测到**的泄漏不该真的留在机器里。"""
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


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
    pid = _read_pid(pid_file)
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
    pid = _read_pid(pid_file)
    assert not _pid_alive(pid)


# ── 外层兜底是**唯一**防线的两条路径（review S2） ────────────────
#
# `start_gateway` 的失败分支只接 `ProbeEnvError`（超时 / 进程早退），自己会
# `_terminate_process()`——于是上面那条用例在"去掉 `start()` 的外层兜底"时仍绿。
# 下面两条让异常**穿过** `start_gateway`（非 `ProbeEnvError` / `BaseException`），
# 此时 `start()` 的 `except BaseException: await env.stop()` 是唯一的收尸人：
# 去掉它，进程就泄漏（AD17 点名的正是这一层）。


async def _explode_with(exc: BaseException, *_: object, **__: object) -> None:
    """``wait_for_health`` 的替身：直接抛给定异常（合成，供兜底分支专用）。"""
    raise exc


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_non_probe_error_from_health_wait_is_reaped_by_the_outer_guard(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``start()`` 中途抛非 ``ProbeEnvError`` ⇒ 外层 ``except BaseException`` 收干净。

    真实触发面（不需要人为构造）：``wait_for_health`` 之外的东西在健康检查阶段
    抛错（例如事件循环被关、第三方库的意外异常）——它们都不该把已起的网关留成孤儿。
    """
    fake_gateway = tmp_path / "fake-gateway"
    fake_gateway.write_text(_FAKE_GATEWAY, encoding="utf-8")
    fake_gateway.chmod(0o755)
    pid_file = tmp_path / "child.pid"
    monkeypatch.setattr("wing_probe.env.DEFAULT_SHUTDOWN_WAIT", 0.2)
    monkeypatch.setattr(
        "wing_probe.env.wait_for_health",
        lambda *args, **kwargs: _explode_with(
            TimeoutError("synthetic"), *args, **kwargs
        ),
    )

    from wing_probe.env import ProbeEnv

    with pytest.raises(TimeoutError):
        await ProbeEnv.start(
            tmp_path / "env",
            gateway_bin=fake_gateway,
            health_timeout=0.3,
            gateway_attempts=1,
            env_overrides={"PROBE_PID_FILE": str(pid_file)},
        )

    pid = _read_pid(pid_file)
    try:
        assert not _pid_alive(pid), (
            f"spawned gateway {pid} leaked after a non-ProbeEnvError failure"
        )
    finally:
        _reap(pid)


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_cancelled_start_still_reaps_the_spawned_gateway(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """取消语义（``BaseException``，``except Exception`` 接不住）同样不留孤儿。

    合成抛 ``CancelledError``：钉的是兜底分支**选了 ``BaseException`` 而不是
    ``Exception``** 这个形状（真实取消下收尾要靠 shield，不在本用例的范围）。
    """
    fake_gateway = tmp_path / "fake-gateway"
    fake_gateway.write_text(_FAKE_GATEWAY, encoding="utf-8")
    fake_gateway.chmod(0o755)
    pid_file = tmp_path / "child.pid"
    monkeypatch.setattr("wing_probe.env.DEFAULT_SHUTDOWN_WAIT", 0.2)
    monkeypatch.setattr(
        "wing_probe.env.wait_for_health",
        lambda *args, **kwargs: _explode_with(
            asyncio.CancelledError(), *args, **kwargs
        ),
    )

    from wing_probe.env import ProbeEnv

    with pytest.raises(asyncio.CancelledError):
        await ProbeEnv.start(
            tmp_path / "env",
            gateway_bin=fake_gateway,
            health_timeout=0.3,
            gateway_attempts=1,
            env_overrides={"PROBE_PID_FILE": str(pid_file)},
        )

    pid = _read_pid(pid_file)
    try:
        assert not _pid_alive(pid), (
            f"spawned gateway {pid} leaked after a cancelled start()"
        )
    finally:
        _reap(pid)
