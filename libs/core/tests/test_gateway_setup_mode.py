"""Setup mode 端到端（04）：降级启动 · 守门 · 修复模式鉴权 · 就地转入正常模式。

用**真** ``GatewayServer`` + TestClient（lifespan 真跑）：本文件要证明的正是
「配置坏掉时网关不再崩，而是活着、只服务设置端点、修好就地转正常模式」这条整机行为，
mock 掉服务器就只剩胶水。``WING_HOME`` 指到 tmp（config.yaml 写在那里），
sessions 走 tmp —— 不碰用户真实的 ``~/.wing``。
"""

from __future__ import annotations

import contextlib
import sys
from collections.abc import Iterator
from pathlib import Path
from unittest.mock import patch

import pytest
import yaml
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from wing.config import ProblemKind, get_config, load_config, reset_config
from wing.config.boot import BootFailure, boot_config
from wing.config.catalog import build_catalog
from wing.config.emit import default_document, emit_config_yaml
from wing.gateway.server import _SetupRuntime
from wing.gateway.setup_guard import SETUP_ALLOWED_PATHS, SetupModeError

#: loopback 来源地址（httpx 的 ``client=`` 会把它写进 ASGI scope）；
#: 默认的 ``TestClient`` 来源是 ``("testclient", 50000)`` —— 天然的非 loopback。
LOOPBACK = ("127.0.0.1", 41000)

#: 合法配置：一个 provider + 一个引用它的 agent。
VALID_YAML = """\
providers:
  - name: local
    base_url: http://127.0.0.1:1/v1
    api_key: k
    models: [m]
agents:
  - name: default
    model: m
"""

#: 非法配置（review 里那种形态）：一个 provider 没声明模型 + agents[0].model 未命中
#: id 空间（目录非空时才会报「未命中」，所以第二个 provider 必须有合法模型）。
INVALID_YAML = """\
providers:
  - name: bad
    base_url: http://127.0.0.1:1/v1
    api_key: k
    models: []
  - name: ok
    base_url: http://127.0.0.1:1/v1
    api_key: k
    models: [m]
agents:
  - name: default
    model: nope
"""

#: YAML 语法错（bad indent）。
SYNTAX_ERROR_YAML = """\
providers:
  - name: bad
   base_url: http://x
"""

#: 修复后的稀疏文档（setup 向导保存的形态）。
GOOD_DOCUMENT = {
    "providers": [
        {
            "name": "p",
            "base_url": "http://127.0.0.1:1/v1",
            "api_key": "k",
            "models": ["m"],
        }
    ],
    "agents": [{"name": "default", "model": "m"}],
}

#: 转入正常模式的六步（与 reload_system 的六项**不是**同一份列表：见 design D3）。
ENTER_ITEMS = [
    "config.yaml",
    "log level",
    "prompt commands",
    "runtime",
    "background jobs",
    "auth",
]


def _wing_home(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, text: str | None
) -> Path:
    """WING_HOME → tmp（可选写入 config.yaml），并清掉 loader 单例（conftest 注入过）。"""
    monkeypatch.setenv("WING_HOME", str(tmp_path))
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    reset_config()
    path = tmp_path / "core" / "config.yaml"
    if text is not None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return path


def _server(text: str | None, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """tmp WING_HOME + 真 ``GatewayServer``（lifespan 由 TestClient 起）。"""
    _wing_home(tmp_path, monkeypatch, text)
    from wing.gateway.server import GatewayServer

    return GatewayServer()


@contextlib.contextmanager
def _client(server, *, loopback: bool = True) -> Iterator[TestClient]:
    """TestClient（lifespan 真跑）：``loopback=True`` 伪造 loopback 来源地址，
    ``False`` 用 TestClient 的默认来源（``testclient``，天然非 loopback）。"""
    if loopback:
        with TestClient(server._app, client=LOOPBACK) as tc:
            yield tc
    else:
        with TestClient(server._app) as tc:
            yield tc


# ============================================================
# boot_config —— 永不抛的启动读取
# ============================================================


class TestBootConfig:
    def test_missing_file_writes_template_and_boots_degraded(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        server = _server(None, tmp_path, monkeypatch)
        path = tmp_path / "core" / "config.yaml"

        assert server.in_setup_mode is True
        assert server.boot_reason is BootFailure.TEMPLATE_CREATED
        # 首次运行 = 向导：模板落盘（emitter 的唯一产物，与保存路径共用）。
        assert path.read_text(encoding="utf-8") == emit_config_yaml(
            default_document(), build_catalog()
        )
        # 模板的天然 problem：providers / agents 空（顺序 = cross_field_problems 的契约）。
        assert [p.path for p in server.setup_problems] == ["agents", "providers"]
        assert {p.kind for p in server.setup_problems} == {ProblemKind.EMPTY_LIST}

    def test_invalid_config_boots_degraded_with_located_problems(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        server = _server(INVALID_YAML, tmp_path, monkeypatch)

        assert server.in_setup_mode is True
        assert server.boot_reason is BootFailure.INVALID
        problems = {p.path: p.kind for p in server.setup_problems}
        assert problems["providers[0].models"] is ProblemKind.MISSING_REQUIRED
        assert problems["agents[0].model"] is ProblemKind.UNKNOWN_REFERENCE

    def test_yaml_syntax_error_reports_the_line_number(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        server = _server(SYNTAX_ERROR_YAML, tmp_path, monkeypatch)

        assert server.in_setup_mode is True
        assert server.boot_reason is BootFailure.PARSE_ERROR
        (problem,) = server.setup_problems
        assert problem.path is None  # 文档级：定位不到单个字段
        with pytest.raises(yaml.YAMLError) as excinfo:
            yaml.safe_load(SYNTAX_ERROR_YAML)
        expected_line = excinfo.value.problem_mark.line + 1  # ty: ignore[unresolved-attribute]
        assert f"第 {expected_line} 行" in problem.message

    def test_boot_config_never_raises_and_keeps_the_loader_contract(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """``boot_config()`` 永不抛；``load_config()`` / ``get_config()`` 仍然抛（对外行为未变）。"""
        _wing_home(tmp_path, monkeypatch, INVALID_YAML)

        boot = boot_config()
        assert boot.ok is False and boot.config is None
        assert boot.path == tmp_path / "core" / "config.yaml"
        assert boot.problems

        # loader 的契约一字未改：非法配置仍然抛 ValueError（不是失败 → 不是退出）。
        with pytest.raises(ValueError, match="Invalid config"):
            load_config()
        with pytest.raises(ValueError, match="Invalid config"):
            get_config()

    def test_host_port_come_from_the_file_even_when_the_config_is_unusable(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ):
        """``gateway.port`` 合法但 ``providers`` 非法 ⇒ 网关仍监听文件里写的端口。

        Rust 侧 ``cmd/backend_config.rs`` 是 ``#[serde(default)]`` 的部分结构，
        读的是同一份文件的同一段——两边必须落在同一个 endpoint 上（总设计 §8.6）。
        """
        _wing_home(tmp_path, monkeypatch, INVALID_YAML + "gateway:\n  port: 39987\n")

        boot = boot_config()
        assert boot.ok is False
        assert boot.reason is BootFailure.INVALID
        assert boot.endpoint == ("127.0.0.1", 39987)  # host 缺省 = 声明默认值

        captured: dict[str, object] = {}
        monkeypatch.setattr(sys, "argv", ["wing-gateway"])
        from wing.gateway import cli

        monkeypatch.setattr(
            cli.GatewayServer,
            "start",
            lambda self: captured.update(host=self.host, port=self.port),
        )
        cli.main()
        assert captured == {"host": "127.0.0.1", "port": 39987}
        # 降级横幅：reason + 前 10 条 problem 的定位 + 修复指引（进的是**终端**，
        # 配置坏掉时用户第一眼就要看到「怎么修」）。
        out = capsys.readouterr().out
        assert "⚠ 配置不可用（invalid）：网关以修复模式启动 127.0.0.1:39987" in out
        assert "providers[0].models" in out
        assert "wing config doctor" in out


# ============================================================
# 守门 —— 白名单可用，其余 503 setup_mode
# ============================================================


class TestSetupGuard:
    def test_allowlist_is_available_and_the_rest_is_503_setup_mode(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        server = _server(INVALID_YAML, tmp_path, monkeypatch)
        path = tmp_path / "core" / "config.yaml"
        before = path.read_bytes()

        with _client(server) as tc:
            for allowed in (
                "/api/health",
                "/api/settings/schema",
                "/api/settings/get",
                "/api/settings/status",
            ):
                resp = tc.get(allowed)
                assert resp.status_code == 200, (allowed, resp.text)

            # /api/settings/set 也在白名单里：非法文档照常给出 ok=false + problems，
            # 且**一个字节都不写**（全有或全无在 setup mode 下同样成立）。
            resp = tc.post(
                "/api/settings/set", json={"base": None, "document": {"providers": []}}
            )
            assert resp.status_code == 200
            assert resp.json()["ok"] is False
            assert resp.json()["problems"]
            assert path.read_bytes() == before

            for blocked, method in (
                ("/api/session/create", "POST"),
                ("/api/session/list", "GET"),
                ("/api/models", "GET"),
                ("/api/agents", "GET"),
                ("/api/commands", "GET"),
                ("/api/tools", "GET"),
                ("/api/system/reload", "POST"),
            ):
                resp = tc.request(method, blocked, json={})
                assert resp.status_code == 503, (blocked, resp.status_code)
                body = resp.json()
                # P4：守门**显式**覆盖 error 码（HTTP_ERROR_TYPES[503] 是通用值）。
                assert body["error"] == "setup_mode", blocked
                assert "agents[0].model" in body["detail"], blocked

        # /api/shutdown 在名单里，但不打它（真 SIGTERM）。
        assert "/api/shutdown" in SETUP_ALLOWED_PATHS

    def test_generic_503_is_not_setup_mode(self):
        """P4：``HTTP_ERROR_TYPES[503]`` 是通用的 service_unavailable。"""
        import json

        from wing.gateway.protocol import error_response

        body = json.loads(bytes(error_response(503, "boom").body).decode("utf-8"))
        assert body["error"] == "service_unavailable"

    def test_ws_refuses_the_handshake_in_setup_mode(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """accept **之前** close ⇒ 客户端看到的是握手失败（不是连上就断）。"""
        server = _server(INVALID_YAML, tmp_path, monkeypatch)
        with _client(server) as tc:
            with pytest.raises(WebSocketDisconnect):
                with tc.websocket_connect("/ws"):
                    pass  # pragma: no cover - 到不了这里

    def test_loopback_repair_access_and_remote_403(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        server = _server(INVALID_YAML, tmp_path, monkeypatch)

        with _client(server, loopback=True) as tc:
            # 修复模式：loopback 免 key（配置坏掉时 auth 配置本身不可信）。
            assert tc.get("/api/settings/status").status_code == 200

        with _client(server, loopback=False) as tc:
            resp = tc.get("/api/settings/status")
            assert resp.status_code == 403
            assert "setup mode" in resp.json()["detail"]
            # loopback 判定先于守门：受限路径也是 403（不是 503）。
            assert tc.get("/api/session/list").status_code == 403

    def test_normal_mode_keeps_the_existing_auth_path(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """正常模式：auth 关闭时非 loopback 照常放行（setup 分支不越界）。"""
        server = _server(VALID_YAML, tmp_path, monkeypatch)
        assert server.in_setup_mode is False

        with _client(server, loopback=False) as tc:
            assert tc.get("/api/health").status_code == 200
            assert tc.get("/api/settings/status").status_code == 200

    def test_setup_runtime_rejects_everything_but_the_save(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        server = _server(INVALID_YAML, tmp_path, monkeypatch)
        assert isinstance(server.runtime, _SetupRuntime)

        with pytest.raises(SetupModeError):
            server.runtime.list_sessions()
        with pytest.raises(SetupModeError):
            _ = server.runtime.sm

    def test_setup_mode_error_maps_to_503_setup_mode(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """万一 ``SetupModeError`` 可达：503 + ``error="setup_mode"``（不是 400/500）。

        正常路径里守门中间件保证它不可达（替身只在保存事务上放行）；这里让替身的
        ⑦ 步直接抛，钉住 app 的异常处理器形状。
        """
        server = _server(INVALID_YAML, tmp_path, monkeypatch)
        with _client(server) as tc:
            with patch.object(
                _SetupRuntime,
                "post_write_effect",
                side_effect=SetupModeError(server.setup_problems),
            ):
                resp = tc.post(
                    "/api/settings/set",
                    json={"base": None, "document": GOOD_DOCUMENT},
                )
            assert resp.status_code == 503
            body = resp.json()
            assert body["error"] == "setup_mode"
            assert "agents[0].model" in body["detail"]


# ============================================================
# 就地转入正常模式
# ============================================================


class TestEnterOperational:
    def test_save_from_setup_mode_enters_operational_in_place(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        server = _server(INVALID_YAML, tmp_path, monkeypatch)
        assert server.in_setup_mode is True

        with _client(server) as tc:
            resp = tc.post(
                "/api/settings/set", json={"base": None, "document": GOOD_DOCUMENT}
            )
            assert resp.status_code == 200, resp.text
            body = resp.json()
            assert body["ok"] is True
            assert body["setup_mode_exited"] is True
            assert body["reload"]["ok"] is True
            assert [item["name"] for item in body["reload"]["results"]] == ENTER_ITEMS

            # 网关就地转入正常模式（**不重启进程**）。
            assert server.in_setup_mode is False
            assert not isinstance(server.runtime, _SetupRuntime)
            assert get_config().providers[0].name == "p"
            assert "session-eviction" in server._background.jobs
            # hooks 已加载（WingRuntime 构造的一部分）+ 后台任务在跑（lifespan 已请求）。
            assert server._background.running is True

            # 守门放行、WS 可连、会话可建。
            assert tc.post("/api/session/create", json={}).status_code == 200
            with tc.websocket_connect("/ws") as ws:
                assert ws.receive_json()["client_id"]

    def test_transition_failure_keeps_setup_mode(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """文件已写（合法配置），但转入失败 ⇒ ok=true / reload.ok=false / 仍在 setup mode。"""
        path = _wing_home(tmp_path, monkeypatch, INVALID_YAML)
        from wing.gateway.server import GatewayServer

        server = GatewayServer()

        with _client(server) as tc:
            with patch(
                "wing.gateway.server.WingRuntime", side_effect=RuntimeError("boom")
            ):
                resp = tc.post(
                    "/api/settings/set", json={"base": None, "document": GOOD_DOCUMENT}
                )
            assert resp.status_code == 200
            body = resp.json()
            assert body["ok"] is True  # 文件写了——它是合法配置
            assert body["setup_mode_exited"] is False
            assert body["reload"]["ok"] is False
            assert [item["name"] for item in body["reload"]["results"]] == ENTER_ITEMS[
                :4
            ]
            assert body["reload"]["results"][-1]["detail"] == "boom"

            # 文件真的写下去了（配置合法）；网关**保持 setup mode**（没有半死状态）。
            written = yaml.safe_load(path.read_text(encoding="utf-8"))
            assert written["providers"][0]["name"] == "p"
            assert server.in_setup_mode is True
            assert tc.post("/api/session/create", json={}).status_code == 503

        # 下一次保存 / 重启再试：清掉故障后能补上。
        with _client(server) as tc:
            resp = tc.post(
                "/api/settings/set", json={"base": None, "document": GOOD_DOCUMENT}
            )
            assert resp.json()["setup_mode_exited"] is True
            assert server.in_setup_mode is False

    def test_partial_transition_failure_rolls_nothing_back(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """②–③ 失败（这里让 prompt commands 炸）也停在 setup mode，且 ① 的产物不回滚。"""
        server = _server(INVALID_YAML, tmp_path, monkeypatch)
        with _client(server) as tc:
            with patch(
                "wing.gateway.server.register_prompt_commands",
                side_effect=RuntimeError("commands"),
            ):
                body = tc.post(
                    "/api/settings/set", json={"base": None, "document": GOOD_DOCUMENT}
                ).json()
            assert body["ok"] is True
            assert body["setup_mode_exited"] is False
            assert [item["name"] for item in body["reload"]["results"]] == [
                "config.yaml",
                "log level",
                "prompt commands",
            ]
            assert server.in_setup_mode is True
