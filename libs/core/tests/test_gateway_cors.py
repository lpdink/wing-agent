# tests/test_gateway_cors.py
"""CORS 装配 —— 默认放通 loopback、外部来源显式配置、预检不被鉴权拦下。

口径（``wing/gateway/cors.py``）：默认放通 loopback 来源（http/https + localhost /
127.0.0.1 / [::1]，任意端口）；外部来源走 ``gateway.cors.origins``（默认空）；
``allow_credentials`` 恒 false（API key 走 header）。

用**真** ``GatewayServer`` + tmp WING_HOME（真 load_config），所以这里钉的就是装配
后的真实中间件链行为——包括「CORS 在鉴权之外」这一条（预检不带凭据）。
"""

from __future__ import annotations

import contextlib
from collections.abc import Iterator
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from wing.config import reset_config

#: 合法配置 + 一个显式配置的外部来源（loopback 之外）。
VALID_YAML = """\
providers:
  - name: local
    base_url: https://api.example.com
    api_key: sk-test
    models: [m1]
agents:
  - name: default
    model: m1
gateway:
  cors:
    origins: ["https://ui.example.com"]
"""

CONFIGURED_ORIGIN = "https://ui.example.com"
LOOPBACK_ORIGINS = [
    "http://localhost:5173",
    "http://127.0.0.1:3000",
    "https://localhost:8443",
    "http://[::1]:8080",
]
FOREIGN_ORIGINS = [
    "https://evil.example.com",
    "http://localhost.evil.example.com",
    "http://127.0.0.1.evil.example.com",
    "https://example.com",
]
ADMIN_KEY = "admin-key-123456"


@contextlib.contextmanager
def _gateway(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, auth_enabled: bool = False
) -> Iterator[TestClient]:
    monkeypatch.setenv("WING_HOME", str(tmp_path))
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    text = VALID_YAML
    if auth_enabled:
        text += f'  auth:\n    enabled: true\n    keys:\n      - {{key: "{ADMIN_KEY}", role: admin}}\n'
    config_path = tmp_path / "core" / "config.yaml"
    config_path.parent.mkdir(parents=True)
    config_path.write_text(text, encoding="utf-8")
    reset_config()

    from wing.gateway.server import GatewayServer

    server = GatewayServer()
    try:
        with TestClient(server._app) as tc:
            yield tc
    finally:
        reset_config()


@pytest.fixture
def client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[TestClient]:
    with _gateway(tmp_path, monkeypatch) as tc:
        yield tc


def _allow_origin(response) -> str | None:
    return response.headers.get("access-control-allow-origin")


class TestSimpleRequests:
    @pytest.mark.parametrize("origin", LOOPBACK_ORIGINS)
    def test_loopback_origins_are_echoed(self, client: TestClient, origin: str) -> None:
        resp = client.get("/api/health", headers={"Origin": origin})
        assert resp.status_code == 200
        assert _allow_origin(resp) == origin

    @pytest.mark.parametrize("origin", FOREIGN_ORIGINS)
    def test_foreign_origins_are_not_allowed_by_default(
        self, client: TestClient, origin: str
    ) -> None:
        resp = client.get("/api/health", headers={"Origin": origin})
        assert resp.status_code == 200
        assert _allow_origin(resp) is None

    def test_configured_origin_is_echoed(self, client: TestClient) -> None:
        resp = client.get("/api/health", headers={"Origin": CONFIGURED_ORIGIN})
        assert resp.status_code == 200
        assert _allow_origin(resp) == CONFIGURED_ORIGIN

    def test_no_origin_header_means_no_cors_headers(self, client: TestClient) -> None:
        resp = client.get("/api/health")
        assert resp.status_code == 200
        assert _allow_origin(resp) is None

    def test_credentials_are_never_allowed(self, client: TestClient) -> None:
        resp = client.get("/api/health", headers={"Origin": "http://localhost:5173"})
        assert "access-control-allow-credentials" not in resp.headers


class TestPreflight:
    def test_preflight_for_loopback_is_answered(self, client: TestClient) -> None:
        resp = client.options(
            "/api/workspace/list",
            headers={
                "Origin": "http://localhost:5173",
                "Access-Control-Request-Method": "GET",
                "Access-Control-Request-Headers": "authorization",
            },
        )
        assert resp.status_code == 200
        assert _allow_origin(resp) == "http://localhost:5173"
        assert "GET" in resp.headers["access-control-allow-methods"]
        assert "authorization" in resp.headers["access-control-allow-headers"].lower()

    def test_preflight_for_foreign_origin_is_refused(self, client: TestClient) -> None:
        resp = client.options(
            "/api/workspace/list",
            headers={
                "Origin": "https://evil.example.com",
                "Access-Control-Request-Method": "GET",
            },
        )
        assert _allow_origin(resp) is None

    def test_preflight_bypasses_authentication(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """预检不带凭据：必须在鉴权**之外**应答，否则浏览器永远发不出真实请求。"""
        with _gateway(tmp_path, monkeypatch, auth_enabled=True) as tc:
            resp = tc.options(
                "/api/workspace/list",
                headers={
                    "Origin": "http://localhost:5173",
                    "Access-Control-Request-Method": "GET",
                    "Access-Control-Request-Headers": "authorization, x-api-key",
                },
            )
            assert resp.status_code == 200
            assert _allow_origin(resp) == "http://localhost:5173"

    def test_auth_rejection_still_carries_cors_headers(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """401 / 403 也要带 ACAO——否则前端读不到错误体（只能看到 CORS 报错）。"""
        with _gateway(tmp_path, monkeypatch, auth_enabled=True) as tc:
            anonymous = tc.get(
                "/api/workspace/roots", headers={"Origin": "http://localhost:5173"}
            )
            assert anonymous.status_code == 401
            assert _allow_origin(anonymous) == "http://localhost:5173"

            admin = tc.get(
                "/api/workspace/roots",
                headers={
                    "Origin": CONFIGURED_ORIGIN,
                    "Authorization": f"Bearer {ADMIN_KEY}",
                },
            )
            assert admin.status_code == 200
            assert _allow_origin(admin) == CONFIGURED_ORIGIN
