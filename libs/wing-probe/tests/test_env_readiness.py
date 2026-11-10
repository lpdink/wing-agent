"""就绪握手：``/api/health`` 的 200 **不代表**应答者是我们起的那个进程。

``reserve_port()`` 是 ``bind(0)`` 后立刻释放，从预留到子进程真正 bind 之间有窗口
（子进程要跑完解释器启动 + ``import wing``，过载的 CI 上以秒计）。窗口里另一个
probe env 拿到同一个端口后，那个端口上听的是别人的网关：``/api/health`` 无鉴权、
照常 200。判错的代价不是红而是**假绿**——整套场景对着别人的 ``WING_HOME`` 跑断言
（还会顺手改掉邻居的 config.yaml）。

所以握手有第二道：身份核对（``GET /api/settings/get`` 的 ``config_path`` 与
``fingerprint``——应答者读的那份 config.yaml 的路径与 sha256，而那份文件是本 env 刚
写下的）。本文件钉住四件事：

1. 别人的网关不会被当成"就绪"（``GatewayPortCollision`` → 换端口重试 → 起不来就抛）；
2. **同一份 ``config_text``** 的两个 env 也不行（指纹相同，只有路径能分开）；
3. 干净环境下走到的是**真核对**（不是被跳过），auth 打开时用的是场景自己的 admin key；
4. ``wait_for_health`` 拿到 200 之后仍会看子进程（它已退出 ⇒ 应答者不是我们）。
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

from wing_probe import GatewayPortCollision, ProbeEnv, ProbeEnvError
from wing_probe.env import config_fingerprint, wait_for_health

#: 一份语法错的配置（网关降级到 setup mode，Setting API 仍可用）。
BROKEN_CONFIG = "gateway: [this is not a mapping\n"


def _pin_reserved_port(monkeypatch: pytest.MonkeyPatch, port: int) -> None:
    """把 ``reserve_port()`` 钉死成 ``port``（``start_gateway`` 按模块全局名查找）。"""
    monkeypatch.setattr("wing_probe.env.reserve_port", lambda host="127.0.0.1": port)


@pytest.mark.asyncio
@pytest.mark.timeout(120)
async def test_a_neighbours_gateway_is_never_reported_as_ready(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """端口被邻居占了 ⇒ 起不来（抛错），而不是"healthy"地连上邻居的网关。

    确定性复现：邻居先正常起来（它的配置路径 / 指纹与我们的必然不同），然后本 env 的
    ``reserve_port()`` 永远返回邻居的端口——重试用尽后必须以 ``ProbeEnvError`` 收场
    （身份不符走 :class:`GatewayPortCollision`，过载下子进程先退则走"200 之后子进程
    已退"那条分支，两条都对），且**没有任何 env 交回给场景**。
    """
    neighbour = await ProbeEnv.start(tmp_path / "neighbour")
    try:
        assert neighbour.port is not None
        fingerprint = config_fingerprint(neighbour.config_path)
        _pin_reserved_port(monkeypatch, neighbour.port)
        env = ProbeEnv(tmp_path / "victim", health_timeout=5.0, gateway_attempts=2)
        try:
            with pytest.raises(ProbeEnvError) as info:
                await env.start_gateway()
        finally:
            await env.stop()

        assert str(neighbour.port) in str(info.value)
        # 邻居毫发无损：还活着、读的还是自己那份配置（收尾不能误杀别人的进程组）。
        assert neighbour.process is not None
        assert neighbour.process.poll() is None
        assert config_fingerprint(neighbour.config_path) == fingerprint
    finally:
        await neighbour.stop()


@pytest.mark.asyncio
@pytest.mark.timeout(120)
async def test_same_config_text_is_not_the_same_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """同一份 ``config_text`` 的两个 env **指纹相同**：只有 ``config_path`` 能分开它们。

    ``config_text`` 逐字落盘、端口由 ``-p`` 覆盖，所以字节（与指纹）可以不唯一；
    只比指纹时这里会静默"就绪"，实际上打在邻居的网关上。
    """
    neighbour = await ProbeEnv.start(tmp_path / "neighbour", config_text=BROKEN_CONFIG)
    try:
        assert neighbour.port is not None
        _pin_reserved_port(monkeypatch, neighbour.port)
        victim = ProbeEnv(
            tmp_path / "victim",
            config_text=BROKEN_CONFIG,
            health_timeout=5.0,
            gateway_attempts=1,
        )
        try:
            with pytest.raises(GatewayPortCollision, match="reads"):
                await victim.start_gateway()
            # 前提（失败之后才成立：配置在每次尝试里写下去）：两个 env 字节相同。
            assert config_fingerprint(
                victim.gateway_config_path()
            ) == config_fingerprint(neighbour.config_path)
        finally:
            await victim.stop()
        assert neighbour.process is not None and neighbour.process.poll() is None
    finally:
        await neighbour.stop()


@pytest.mark.asyncio
@pytest.mark.timeout(120)
async def test_identity_check_runs_on_a_healthy_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """干净环境下身份核对真的跑了（不是被跳过）：把核对结果改成不符 ⇒ 起不来。

    不改端口、只让"期望指纹"错位——如果实现里核对被静默跳过，这个 env 会正常起来，
    用例立刻红。
    """
    calls: list[str] = []
    real = ProbeEnv._verify_identity

    async def _verify(self: ProbeEnv) -> None:
        calls.append(str(self._port))
        await real(self)

    monkeypatch.setattr(ProbeEnv, "_verify_identity", _verify)
    env = await ProbeEnv.start(tmp_path / "env")
    try:
        assert calls[-1] == str(env.port), "就绪握手没有跑身份核对"
    finally:
        await env.stop()

    # 期望指纹错位 = 应答者"读的是别人的文件" ⇒ 必须判为端口被抢。
    monkeypatch.setattr("wing_probe.env.config_fingerprint", lambda path: "0" * 64)
    misaligned = ProbeEnv(tmp_path / "env2", health_timeout=5.0, gateway_attempts=1)
    try:
        with pytest.raises(ProbeEnvError, match="taken over"):
            await misaligned.start_gateway()
    finally:
        await misaligned.stop()


@pytest.mark.asyncio
@pytest.mark.timeout(120)
async def test_identity_check_uses_the_admin_key_when_auth_is_on(
    tmp_path: Path,
) -> None:
    """auth 打开时核对带的是场景自己写下的 admin key（不带 ⇒ 401 ⇒ 起不来）。"""
    env = await ProbeEnv.start(
        tmp_path / "env",
        auth={"enabled": True, "keys": [{"key": "probe-admin-key"}]},
    )
    try:
        assert env.process is not None and env.process.poll() is None
    finally:
        await env.stop()


@pytest.mark.asyncio
@pytest.mark.timeout(120)
async def test_identity_check_is_skipped_without_an_admin_key(
    tmp_path: Path,
) -> None:
    """只有 tool_runtime key（读不了 Setting API）⇒ 跳过核对，不制造假红。"""
    env = await ProbeEnv.start(
        tmp_path / "env",
        auth={
            "enabled": True,
            "keys": [{"key": "probe-tool-key", "role": "tool_runtime"}],
        },
    )
    try:
        assert env.process is not None and env.process.poll() is None
    finally:
        await env.stop()


class _ProcessStub:
    """``Popen`` 的最小替身：``poll()`` 按序返回给定值（越界后重复最后一个）。"""

    def __init__(self, *codes: int | None) -> None:
        self._codes = list(codes)
        self.returncode: int | None = None

    def poll(self) -> int | None:
        code = self._codes.pop(0) if len(self._codes) > 1 else self._codes[0]
        self.returncode = code
        return code


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_health_200_from_a_dead_child_is_not_ready(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """200 之后再看一眼子进程：它已退出 ⇒ 那个 200 不是它给的。"""
    import httpx

    async def _fake_get(self: Any, url: str, **kwargs: Any) -> Any:
        return httpx.Response(
            200, json={"status": "ok"}, request=httpx.Request("GET", url)
        )

    monkeypatch.setattr(httpx.AsyncClient, "get", _fake_get)
    # 第一次 poll（GET 之前）还活着，第二次（200 之后）已经退出 1。
    process = _ProcessStub(None, 1)

    with pytest.raises(ProbeEnvError, match="another process"):
        await wait_for_health(
            "http://127.0.0.1:1/api/health",
            process=process,  # ty: ignore[invalid-argument-type]
            timeout=5.0,
            log_path=tmp_path / "gateway.log",
        )


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_health_200_runs_the_identity_hook(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """200 之后必须调身份钩子（钩子抛错 ⇒ 就绪判定失败，不外溢成"已就绪"）。"""
    import httpx

    async def _fake_get(self: Any, url: str, **kwargs: Any) -> Any:
        return httpx.Response(
            200, json={"status": "ok"}, request=httpx.Request("GET", url)
        )

    monkeypatch.setattr(httpx.AsyncClient, "get", _fake_get)
    seen: list[str] = []

    async def _identity() -> None:
        seen.append("called")
        await asyncio.sleep(0)
        raise GatewayPortCollision("not ours")

    with pytest.raises(GatewayPortCollision):
        await wait_for_health("http://127.0.0.1:1/api/health", identity=_identity)
    assert seen == ["called"]
