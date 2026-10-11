"""GatewayServer 生命周期与路由骨架：端口探测、start()、auth 告警、setup 边角。

setup mode 的完整语义在 test_gateway_setup_mode.py；这里只补生命周期外壳：
进入正常模式的幂等/失败提前返回、auth 锁死告警、`start()` 的端口占用退出、
以及事件路由对「无 target / 不接收事件的 client」的短路。
"""

from __future__ import annotations

import socket
import sys
from pathlib import Path

import pytest

from wing.config import ApiKeyEntry, AuthConfig

pytestmark = pytest.mark.asyncio


def _server(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, text: str | None = None):
    """tmp WING_HOME + 真 GatewayServer（text=None ⇒ setup mode 降级启动）。"""
    from wing.config import reset_config

    monkeypatch.setenv("WING_HOME", str(tmp_path))
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    reset_config()
    if text is not None:
        path = tmp_path / "core" / "config.yaml"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    from wing.gateway.server import GatewayServer

    return GatewayServer()


MINIMAL_YAML = """\
providers:
  - name: p
    base_url: http://127.0.0.1:1/v1
    api_key: k
    models: [m]
agents:
  - name: default
    model: m
"""


# ── 端口探测 ──────────────────────────────────────────────────


class TestPortAvailability:
    async def test_free_port_is_available(self):
        from wing.gateway.server import _check_port_available

        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            free_port = probe.getsockname()[1]

        assert _check_port_available("127.0.0.1", free_port) is True

    async def test_occupied_port_is_reported_unavailable(self):
        from wing.gateway.server import _check_port_available

        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            port = listener.getsockname()[1]

            assert _check_port_available("127.0.0.1", port) is False


# ── start() ───────────────────────────────────────────────────


class TestStart:
    async def test_start_refuses_when_port_is_taken(
        self, tmp_path, monkeypatch, capsys
    ):
        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)
        monkeypatch.setattr(
            "wing.gateway.server._check_port_available", lambda host, port: False
        )
        monkeypatch.setattr(
            "wing.gateway.server.uvicorn.run",
            lambda *a, **kw: pytest.fail("must not start uvicorn on a taken port"),
        )

        with pytest.raises(SystemExit) as excinfo:
            server.start()

        assert excinfo.value.code == 1
        assert "已被占用" in capsys.readouterr().out

    async def test_start_subscribes_and_runs_uvicorn(
        self, tmp_path, monkeypatch, capsys
    ):
        from wing.event_bus import event_bus

        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)
        monkeypatch.setattr(
            "wing.gateway.server._check_port_available", lambda host, port: True
        )
        runs: list[dict] = []
        monkeypatch.setattr(
            "wing.gateway.server.uvicorn.run",
            lambda app, **kwargs: runs.append({"app": app, **kwargs}),
        )

        server.start()

        assert runs and runs[0]["app"] is server._app
        assert (runs[0]["host"], runs[0]["port"]) == (server.host, server.port)
        assert runs[0]["log_config"] is None  # 日志由 wing 自己的 handler 出
        assert server._on_event in event_bus._subscribers
        assert "🚀 Gateway 启动于" in capsys.readouterr().out


# ── auth ──────────────────────────────────────────────────────


class TestAuthWarnings:
    async def test_lockout_warning_when_no_keys(self, tmp_path, monkeypatch):
        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)
        # 直接替换模块日志对象：GatewayServer 构造时装好的 logger 会清掉外部
        # handler，挂 conftest 的捕获器在这里抓不到东西。
        logged: list[str] = []
        server_mod = sys.modules["wing.gateway.server"]
        monkeypatch.setattr(
            server_mod, "log", type("_Rec", (), {"warning": logged.append})()
        )

        message = server._warn_auth_lockout(
            AuthConfig(enabled=True, keys=[], loopback_exempt=False)
        )

        assert message is not None and "ALL requests" in message
        assert logged == [message]

    async def test_no_warning_when_keys_present_or_disabled(
        self, tmp_path, monkeypatch
    ):
        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)
        with_key = AuthConfig(
            enabled=True,
            keys=[ApiKeyEntry(key="k", role="admin")],
            loopback_exempt=False,
        )

        assert server._warn_auth_lockout(with_key) is None
        assert server._warn_auth_lockout(AuthConfig(enabled=False)) is None

    async def test_default_source_is_current_config(self, tmp_path, monkeypatch):
        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)
        # 未传参时读当前配置（此处 auth 默认关闭 → 无告警）
        assert server._warn_auth_lockout() is None

    async def test_setup_mode_auth_config_is_safe_default(self, tmp_path, monkeypatch):
        """配置读不出来时读 auth 不得炸：返回安全默认值（守门由中间件负责）。"""
        server = _server(tmp_path, monkeypatch, text=None)

        assert server.in_setup_mode is True
        assert server.auth_config.enabled is False
        assert server.auth_config.keys == []


# ── 进入正常模式 ───────────────────────────────────────────────


class TestEnterOperational:
    async def test_idempotent_when_already_operational(self, tmp_path, monkeypatch):
        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)

        result = server._enter_operational()

        assert result.ok is True
        assert result.items == []

    async def test_config_reload_failure_keeps_setup_mode(self, tmp_path, monkeypatch):
        server = _server(tmp_path, monkeypatch, text=None)
        assert server.in_setup_mode is True

        def _boom(*args, **kwargs):
            raise RuntimeError("disk on fire")

        monkeypatch.setattr("wing.gateway.server.load_config", _boom)

        result = server._enter_operational()

        assert result.ok is False
        assert [(i.name, i.ok) for i in result.items] == [("config.yaml", False)]
        assert "disk on fire" in result.items[0].detail
        assert server.in_setup_mode is True


# ── 事件路由短路 ───────────────────────────────────────────────


class TestOnEventRouting:
    async def test_event_without_target_is_dropped(self, tmp_path, monkeypatch):
        from wing.event import TextEvent

        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)
        sent: list = []
        server._schedule_send = lambda *a, **kw: sent.append(a)  # ty: ignore[invalid-assignment]

        server._on_event(TextEvent(content="no target"))

        assert sent == []

    async def test_global_event_skips_clients_without_subscription(
        self, tmp_path, monkeypatch
    ):
        from wing.event import EventTarget, TextEvent

        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)
        sent: list = []
        server._schedule_send = lambda *a, **kw: sent.append(a)  # ty: ignore[invalid-assignment]
        subscribed = object()
        silent = object()
        server._client_to_ws = {"c1": subscribed, "c2": silent}  # ty: ignore[invalid-assignment]
        monkeypatch.setattr(
            server, "_receives_events", lambda cid: cid == "c1", raising=False
        )

        event = TextEvent(content="broadcast", target=EventTarget(scope="global"))
        server._on_event(event)

        assert len(sent) == 1
        # 全局帧带切分后的帧数组（wire 出口统一切分）
        frames = sent[0][1]
        assert isinstance(frames, list) and frames


# ── 回收边界 ──────────────────────────────────────────────────


class TestRecycleEdges:
    async def test_recycle_unknown_ws_is_noop(self, tmp_path, monkeypatch):
        server = _server(tmp_path, monkeypatch, MINIMAL_YAML)

        await server._recycle_client(object(), reason="not tracked")  # ty: ignore[invalid-argument-type]

        assert server.clients == {}
