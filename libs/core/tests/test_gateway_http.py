"""Gateway HTTP 端点单元测试。

使用 FastAPI TestClient + Mock WingRuntime 测试所有 HTTP 路由。
不启动真实 uvicorn server，不依赖端口 32523。

例外：``TestRealRuntimeGates`` 用**真实** ``WingRuntime``——它要证明的是
"闸门在 HTTP 面上把恶意 id 折成 400（而不是让它走到响应序列化处炸 500）"，
mock 掉 runtime 就只剩 route 的胶水，证明不了这件事。
"""

from __future__ import annotations

import json
import re
from datetime import datetime
from importlib.metadata import PackageNotFoundError
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from wing.config import ApiKeyEntry, AuthConfig
from wing.event import AgentInfo, SessionInfo
from wing.gateway.routes import health as health_route
from wing.session import TagMutation
from wing.store import TagMeta


def _raise_package_not_found(name: str) -> str:
    """替身 version()：任何包名都不存在。"""
    raise PackageNotFoundError(name)


def _mock_config(auth_enabled: bool = False, auth_keys: list | None = None):
    """构建 mock Config，仅填充 gateway.auth 字段。"""
    config = MagicMock()
    config.gateway.auth = AuthConfig(
        enabled=auth_enabled,
        keys=auth_keys or [],
    )
    return config


@pytest.fixture
def mock_runtime():
    """创建 mock WingRuntime，避免初始化真实 LLM 连接。"""
    runtime = MagicMock()

    # create_session 默认返回 mock session
    mock_session = MagicMock()
    mock_session.session_id = "test-session-id"
    mock_session.template_name = "default"
    mock_session.session_workspace = "/tmp"
    mock_session.store.name = "file"
    runtime.create_session.return_value = mock_session

    # resume_session 默认返回 mock session
    runtime.resume_session.return_value = mock_session

    # ensure_loaded（逐出/水合入口）默认返回 mock session
    runtime.ensure_loaded.return_value = mock_session

    # fork_session 默认返回 (new_session, draft)
    new_session = MagicMock()
    new_session.session_id = "forked-session-id"
    runtime.fork_session.return_value = (new_session, "my draft")

    # list_sessions 返回空列表
    runtime.list_sessions.return_value = []

    # get_session_state 返回 mock 状态
    runtime.get_session_state.return_value = {
        "session_id": "test-session-id",
        "name": None,
        "template_name": "default",
        "workspace": "/tmp",
        "messages": [],
        "agent": AgentInfo(
            model_name="gpt-4",
            system_prompt=None,
            tools=[],
            skills=[],
            rules=[],
            workspace="/tmp",
        ),
    }

    # subscribe/unsubscribe 默认成功
    runtime.subscribe.return_value = None
    runtime.unsubscribe.return_value = None

    # post 默认成功（async mock）
    async def mock_post(**kwargs):
        pass

    runtime.post = mock_post

    return runtime


@pytest.fixture
def client(mock_runtime):
    """创建 TestClient，注入 mock runtime。"""
    # 需要 mock WingRuntime.__init__ 和 load_config 避免真实初始化
    with (
        patch("wing.gateway.server.WingRuntime") as MockRuntime,
        patch("wing.gateway.server.load_config") as mock_load_config,
    ):
        MockRuntime.return_value = mock_runtime
        mock_load_config.return_value = _mock_config()

        from wing.gateway.server import GatewayServer

        server = GatewayServer()
        # 注入 mock runtime（覆盖 WingRuntime() 创建的真实实例）
        server.runtime = mock_runtime
        # 模拟一个已连接的 client（用于 subscribe/unsubscribe 测试）
        mock_ws = MagicMock()
        server.clients["test-client-id"] = mock_ws

        app = server._app
        with TestClient(app) as tc:
            yield tc


# ============================================================
# 6.1 Health 端点
# ============================================================


class TestHealth:
    """GET /api/health 测试。"""

    def test_health_ok(self, client: TestClient):
        """健康检查返回 200 + status=ok + version + commit。"""
        resp = client.get("/api/health")
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "ok"
        assert data["version"]
        assert data["commit"] is None or re.fullmatch(r"[0-9a-f]{7}", data["commit"])

    def test_version_prefers_build_info(self, monkeypatch: pytest.MonkeyPatch):
        """版本解析：构建信息（构建时注入）优先于发行包元数据。"""
        monkeypatch.setattr(
            health_route, "get_version", lambda: "0.4.1.dev9+g3e6e47256"
        )
        monkeypatch.setattr(health_route, "version", _raise_package_not_found)
        assert health_route._get_version() == "0.4.1.dev9+g3e6e47256"

    def test_version_falls_back_to_dist_metadata(self, monkeypatch: pytest.MonkeyPatch):
        """无构建信息（旧安装缺生成文件）→ 回落 wing-agent 元数据。"""
        monkeypatch.setattr(health_route, "get_version", lambda: None)
        monkeypatch.setattr(health_route, "version", lambda name: "0.4.1")
        assert health_route._get_version() == "0.4.1"

    def test_version_falls_back_to_gateway_dist(self, monkeypatch: pytest.MonkeyPatch):
        """没有 wing-agent（如 pip install libs/core）→ 回落 wing-gateway 元数据。"""
        monkeypatch.setattr(health_route, "get_version", lambda: None)

        def fake_version(name: str) -> str:
            if name == "wing-agent":
                raise PackageNotFoundError(name)
            return "0.4.1"

        monkeypatch.setattr(health_route, "version", fake_version)
        assert health_route._get_version() == "0.4.1"

    def test_version_unknown_falls_back_to_dev(self, monkeypatch: pytest.MonkeyPatch):
        """构建信息与元数据都拿不到 → dev。"""
        monkeypatch.setattr(health_route, "get_version", lambda: None)
        monkeypatch.setattr(health_route, "version", _raise_package_not_found)
        assert health_route._get_version() == "dev"


# ============================================================
# 6.2 Session Create
# ============================================================


class TestSessionCreate:
    """POST /api/session/create 测试。"""

    def test_create_default(self, client: TestClient, mock_runtime):
        """默认模板创建 session。"""
        resp = client.post("/api/session/create", json={})
        assert resp.status_code == 200
        data = resp.json()
        assert data["session_id"] == "test-session-id"
        assert data["template_name"] == "default"
        assert data["backend"] == "file"
        mock_runtime.create_session.assert_called_once_with(
            template_name=None,
            workspace=None,
            agent_override=None,
            backend=None,
            tags=None,
            session_id=None,
        )

    def test_create_with_template(self, client: TestClient, mock_runtime):
        """指定模板创建 session。"""
        resp = client.post(
            "/api/session/create",
            json={"template_name": "coder", "workspace": "/ws"},
        )
        assert resp.status_code == 200
        mock_runtime.create_session.assert_called_once_with(
            template_name="coder",
            workspace="/ws",
            agent_override=None,
            backend=None,
            tags=None,
            session_id=None,
        )

    def test_create_template_not_found(self, client: TestClient, mock_runtime):
        """模板不存在返回 400。"""
        mock_runtime.create_session.side_effect = ValueError("template 'xxx' not found")
        resp = client.post("/api/session/create", json={"template_name": "xxx"})
        assert resp.status_code == 400

    def test_create_with_backend(self, client: TestClient, mock_runtime):
        """backend 参数透传。"""
        resp = client.post("/api/session/create", json={"backend": "memory"})
        assert resp.status_code == 200
        mock_runtime.create_session.assert_called_once_with(
            template_name=None,
            workspace=None,
            agent_override=None,
            backend="memory",
            tags=None,
            session_id=None,
        )

    def test_create_unknown_backend_returns_400(self, client: TestClient, mock_runtime):
        """未知 backend 返回 400。"""
        mock_runtime.create_session.side_effect = ValueError(
            "Unknown storage backend 'redis'. Available: ['file', 'memory']"
        )
        resp = client.post("/api/session/create", json={"backend": "redis"})
        assert resp.status_code == 400
        assert "redis" in resp.json()["detail"]

    def test_create_with_tags(self, client: TestClient, mock_runtime):
        """tags 透传（创建即打标）；非法标签由 runtime raise → 400。"""
        resp = client.post(
            "/api/session/create", json={"tags": ["scheduler", "task=x"]}
        )
        assert resp.status_code == 200
        mock_runtime.create_session.assert_called_once_with(
            template_name=None,
            workspace=None,
            agent_override=None,
            backend=None,
            tags=["scheduler", "task=x"],
            session_id=None,
        )

        mock_runtime.create_session.side_effect = ValueError("invalid tag: 'bad tag'")
        resp = client.post("/api/session/create", json={"tags": ["bad tag"]})
        assert resp.status_code == 400
        assert "bad tag" in resp.json()["detail"]

    def test_create_with_session_id(self, client: TestClient, mock_runtime):
        """指定 session_id 透传（create-or-adopt 的请求面）。"""
        resp = client.post(
            "/api/session/create",
            json={"session_id": "3f2b9d1e-6c1a-4f2b-9d3e-1a2b3c4d5e6f"},
        )
        assert resp.status_code == 200
        mock_runtime.create_session.assert_called_once_with(
            template_name=None,
            workspace=None,
            agent_override=None,
            backend=None,
            tags=None,
            session_id="3f2b9d1e-6c1a-4f2b-9d3e-1a2b3c4d5e6f",
        )

    def test_create_invalid_session_id_is_400(self, client: TestClient, mock_runtime):
        """不合规的 session_id → runtime raise ValueError → 400（明确文案）。"""
        mock_runtime.create_session.side_effect = ValueError(
            "invalid session id: '../escape'"
        )
        resp = client.post("/api/session/create", json={"session_id": "../escape"})
        assert resp.status_code == 400
        assert "invalid session id" in resp.json()["detail"]

    def test_create_with_agent_override(self, client: TestClient, mock_runtime):
        """创建 session 时传入 agent override。"""
        resp = client.post(
            "/api/session/create",
            json={
                "workspace": "/ws",
                "agent": {
                    "model_id": "gpt-4o",
                    "tools": ["Read", "Bash"],
                    "max_turns": 10,
                    "effort": "high",
                },
            },
        )
        assert resp.status_code == 200
        call_kwargs = mock_runtime.create_session.call_args
        override = call_kwargs.kwargs["agent_override"]
        assert override is not None
        assert override.model_id == "gpt-4o"
        assert override.tools == ["Read", "Bash"]
        assert override.max_turns == 10
        assert override.effort == "high"
        assert override.system_prompt is None
        assert override.append_system_prompt is None

    def test_create_with_partial_override(self, client: TestClient, mock_runtime):
        """创建 session 时只覆盖部分字段。"""
        resp = client.post(
            "/api/session/create",
            json={
                "template_name": "default",
                "agent": {
                    "model_id": "claude-sonnet-4-20250514",
                    "append_system_prompt": "\nAlways respond in Chinese.",
                },
            },
        )
        assert resp.status_code == 200
        call_kwargs = mock_runtime.create_session.call_args
        override = call_kwargs.kwargs["agent_override"]
        assert override.model_id == "claude-sonnet-4-20250514"
        assert override.append_system_prompt == "\nAlways respond in Chinese."
        assert override.system_prompt is None
        assert override.tools is None
        assert override.max_turns is None
        assert override.effort is None


# ============================================================
# 6.3 Session Resume
# ============================================================


class TestSessionResume:
    """POST /api/session/resume 测试。"""

    def test_resume_ok(self, client: TestClient, mock_runtime):
        """成功恢复 session。"""
        resp = client.post("/api/session/resume", json={"session_id": "abc123"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["session_id"] == "test-session-id"
        mock_runtime.resume_session.assert_called_once_with(
            "abc123", agent_override=None
        )

    def test_resume_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.resume_session.side_effect = LookupError("Session not found: xxx")
        resp = client.post("/api/session/resume", json={"session_id": "xxx"})
        assert resp.status_code == 404

    def test_resume_passes_agent_override(self, client: TestClient, mock_runtime):
        """resume 的 agent 覆盖透传到 runtime（resume 子集由 Session 决定）。"""
        resp = client.post(
            "/api/session/resume",
            json={
                "session_id": "abc123",
                "agent": {"model_id": "gpt-4o", "effort": "high", "tools": ["Read"]},
            },
        )
        assert resp.status_code == 200
        override = mock_runtime.resume_session.call_args.kwargs["agent_override"]
        assert override is not None
        assert override.model_id == "gpt-4o"
        assert override.effort == "high"
        assert override.tools == ["Read"]
        assert override.system_prompt is None

    def test_resume_override_validation_error_is_400(
        self, client: TestClient, mock_runtime
    ):
        """覆盖里的工具引用无法解析 → runtime raise ValueError → 400（不是 404）。"""
        mock_runtime.resume_session.side_effect = ValueError(
            "cannot resolve tool reference: 'Nope'"
        )
        resp = client.post(
            "/api/session/resume",
            json={"session_id": "abc123", "agent": {"tools": ["Nope"]}},
        )
        assert resp.status_code == 400
        assert "Nope" in resp.json()["detail"]


# ============================================================
# 6.4 Session Fork
# ============================================================


class TestSessionFork:
    """POST /api/session/fork 测试。"""

    def test_fork_ok(self, client: TestClient, mock_runtime):
        """成功分叉 session。"""
        resp = client.post(
            "/api/session/fork",
            json={
                "source_session_id": "src-id",
                "target_uuid": "uuid-123",
            },
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["session_id"] == "forked-session-id"
        assert data["draft"] == "my draft"

    def test_fork_session_not_found(self, client: TestClient, mock_runtime):
        """源 session 不存在返回 404。"""
        mock_runtime.fork_session.side_effect = LookupError(
            "Fork failed: source session 'xxx' not found or target_uuid 'yyy' invalid"
        )
        resp = client.post(
            "/api/session/fork",
            json={"source_session_id": "xxx", "target_uuid": "yyy"},
        )
        assert resp.status_code == 404

    def test_fork_uuid_not_found(self, client: TestClient, mock_runtime):
        """目标 UUID 不存在返回 404。"""
        mock_runtime.fork_session.side_effect = LookupError(
            "target_uuid 'invalid-uuid' invalid"
        )
        resp = client.post(
            "/api/session/fork",
            json={
                "source_session_id": "valid",
                "target_uuid": "invalid-uuid",
            },
        )
        assert resp.status_code == 404


# ============================================================
# 6.5 Session Subscribe
# ============================================================


class TestSessionSubscribe:
    """POST /api/session/subscribe 测试。"""

    def test_subscribe_ok(self, client: TestClient, mock_runtime):
        """成功订阅。"""
        resp = client.post(
            "/api/session/subscribe",
            json={"session_id": "test-session-id"},
            headers={"X-Client-Id": "test-client-id"},
        )
        assert resp.status_code == 200
        assert resp.json()["ok"] is True
        mock_runtime.subscribe.assert_called_once_with(
            "test-client-id", "test-session-id"
        )

    def test_subscribe_missing_header(self, client: TestClient):
        """缺少 X-Client-Id header 返回 400。"""
        resp = client.post(
            "/api/session/subscribe",
            json={"session_id": "test-session-id"},
        )
        assert resp.status_code == 400

    def test_subscribe_client_not_connected(self, client: TestClient):
        """Client 未连接返回 400。"""
        resp = client.post(
            "/api/session/subscribe",
            json={"session_id": "test-session-id"},
            headers={"X-Client-Id": "unknown-client"},
        )
        assert resp.status_code == 400

    def test_subscribe_session_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.subscribe.side_effect = LookupError("Session not found: xxx")
        resp = client.post(
            "/api/session/subscribe",
            json={"session_id": "xxx"},
            headers={"X-Client-Id": "test-client-id"},
        )
        assert resp.status_code == 404


# ============================================================
# 6.6 Session Unsubscribe
# ============================================================


class TestSessionUnsubscribe:
    """POST /api/session/unsubscribe 测试。"""

    def test_unsubscribe_ok(self, client: TestClient, mock_runtime):
        """成功取消订阅。"""
        resp = client.post(
            "/api/session/unsubscribe",
            json={"session_id": "test-session-id"},
            headers={"X-Client-Id": "test-client-id"},
        )
        assert resp.status_code == 200
        assert resp.json()["ok"] is True

    def test_unsubscribe_missing_header(self, client: TestClient):
        """缺少 X-Client-Id header 返回 400。"""
        resp = client.post(
            "/api/session/unsubscribe",
            json={"session_id": "test-session-id"},
        )
        assert resp.status_code == 400


# ============================================================
# 6.7 Session Send
# ============================================================


class TestSessionSend:
    """POST /api/session/send 测试。"""

    def test_send_ok(self, client: TestClient, mock_runtime):
        """成功发送消息。"""
        resp = client.post(
            "/api/session/send",
            json={
                "session_id": "test-session-id",
                "content": "hello",
            },
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert "request_id" in data

    def test_send_session_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在（内存与磁盘都没有）返回 404。"""
        mock_runtime.ensure_loaded.side_effect = LookupError("session not found")
        resp = client.post(
            "/api/session/send",
            json={"session_id": "xxx", "content": "hello"},
        )
        assert resp.status_code == 404

    def test_send_hydrates_evicted_session(self, client: TestClient, mock_runtime):
        """被逐出（不在内存）的会话按需水合后照常投递。"""
        resp = client.post(
            "/api/session/send",
            json={"session_id": "evicted-session-id", "content": "hello"},
        )
        assert resp.status_code == 200
        mock_runtime.ensure_loaded.assert_called_with("evicted-session-id")


# ============================================================
# 6.9 Session Release（逐出内存态）
# ============================================================


class TestSessionRelease:
    """POST /api/session/release 测试。"""

    def test_release_ok(self, client: TestClient, mock_runtime):
        """逐出成功 → released=true。"""
        mock_runtime.release_session.return_value = (True, "released")
        resp = client.post(
            "/api/session/release", json={"session_id": "test-session-id"}
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data == {"ok": True, "released": True, "detail": "released"}

    def test_release_not_loaded_is_idempotent(self, client: TestClient, mock_runtime):
        """会话本就不在内存 → 200 + released=false（幂等，不是错误）。"""
        mock_runtime.release_session.return_value = (False, "not loaded")
        resp = client.post(
            "/api/session/release", json={"session_id": "test-session-id"}
        )
        assert resp.status_code == 200
        assert resp.json()["released"] is False

    def test_release_unknown_session_404(self, client: TestClient, mock_runtime):
        """内存与磁盘都没有该会话 → 404。"""
        mock_runtime.release_session.side_effect = LookupError("session not found")
        resp = client.post("/api/session/release", json={"session_id": "xxx"})
        assert resp.status_code == 404

    def test_release_pinned_session_409(self, client: TestClient, mock_runtime):
        """被钉住（忙碌 / 被订阅 / 非持久后端）→ 409，附原因。"""
        mock_runtime.release_session.side_effect = RuntimeError(
            "session cannot be released: status=working"
        )
        resp = client.post(
            "/api/session/release", json={"session_id": "test-session-id"}
        )
        assert resp.status_code == 409
        assert "status=working" in resp.json()["detail"]


# ============================================================
# 6.8 Session List + Get
# ============================================================


class TestSessionList:
    """GET /api/session/list 测试。"""

    def test_list_empty(self, client: TestClient, mock_runtime):
        """空列表。"""
        resp = client.get("/api/session/list")
        assert resp.status_code == 200
        data = resp.json()
        assert data["sessions"] == []

    def test_list_with_sessions(self, client: TestClient, mock_runtime):
        """返回 session 列表。"""
        mock_runtime.list_sessions.return_value = [
            SessionInfo(
                id="s1",
                name="test",
                template_name="default",
                workspace="/tmp",
                created_at=datetime(2025, 1, 1),
                last_interaction="2025-01-02",
            ),
        ]
        resp = client.get("/api/session/list")
        assert resp.status_code == 200
        data = resp.json()
        assert len(data["sessions"]) == 1
        assert data["sessions"][0]["id"] == "s1"

    def test_list_includes_status(self, client: TestClient, mock_runtime):
        """列表项携带运行时 status。"""
        mock_runtime.list_sessions.return_value = [
            SessionInfo(id="s1", name="a", status="working"),
            SessionInfo(id="s2", name="b", status="inactive"),
        ]
        resp = client.get("/api/session/list")
        assert resp.status_code == 200
        data = resp.json()
        statuses = {s["id"]: s["status"] for s in data["sessions"]}
        assert statuses == {"s1": "working", "s2": "inactive"}


class TestSessionGet:
    """GET /api/session/get 测试。"""

    def test_get_ok(self, client: TestClient, mock_runtime):
        """成功获取 session 状态。"""
        resp = client.get("/api/session/get", params={"session_id": "test-session-id"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["session_id"] == "test-session-id"
        assert data["agent"]["model_name"] == "gpt-4"

    def test_get_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.get_session_state.return_value = None
        resp = client.get("/api/session/get", params={"session_id": "xxx"})
        assert resp.status_code == 404

    def test_get_includes_status(self, client: TestClient, mock_runtime):
        """详情响应携带运行时 status。"""
        mock_runtime.get_session_state.return_value = {
            "session_id": "test-session-id",
            "name": None,
            "template_name": "default",
            "workspace": "/tmp",
            "status": "waiting",
            "messages": [],
            "agent": None,
        }
        resp = client.get("/api/session/get", params={"session_id": "test-session-id"})
        assert resp.status_code == 200
        assert resp.json()["status"] == "waiting"


# ============================================================
# Session Info
# ============================================================


class TestSessionInfo:
    """GET /api/session/info 测试。"""

    def test_info_ok(self, client: TestClient, mock_runtime):
        """正常获取 session 运行时状态。"""
        mock_session = MagicMock()
        mock_session.agent.get_status.return_value = {
            "model": "gpt-4o",
            "api_url": "https://api.openai.com",
            "tools": ["Bash", "Read"],
            "total_tokens": 1000,
            "context_window_tokens": 128000,
            "thinking": True,
            "reasoning_effort": "high",
        }
        mock_session.model_id = "gpt-4o"
        mock_session.agent.provider_name = "default"
        mock_session.agent.model_display_name = "GPT-4o Flash"
        mock_session.agent.yolo = False
        mock_session.session_name = "Test Session"
        mock_session.session_workspace = "/tmp/ws"
        mock_session.status = "idle"
        mock_session.agent.context_manager.get_context_stats.return_value = (5, 800)
        mock_session.agent.context_manager.get_skills_info.return_value = (
            "skill-a: desc"
        )
        mock_session.agent.context_manager.system_prompt.content = (
            "You are a helpful assistant."
        )
        mock_runtime.get_session.return_value = mock_session
        mock_session.tag_meta = {}

        resp = client.get("/api/session/info", params={"session_id": "test-id"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["model"] == "gpt-4o"
        assert data["model_id"] == "gpt-4o"
        assert data["provider_name"] == "default"
        assert data["model_display_name"] == "GPT-4o Flash"
        assert data["api_url"] == "https://api.openai.com"
        assert data["tools"] == ["Bash", "Read"]
        assert data["total_tokens"] == 1000
        assert data["context_window_tokens"] == 128000
        assert data["thinking"] is True
        assert data["reasoning_effort"] == "high"
        assert data["yolo"] is False
        assert data["session_name"] == "Test Session"
        assert data["workdir"] == "/tmp/ws"
        assert data["status"] == "idle"
        assert data["context_stats"]["message_count"] == 5
        assert data["context_stats"]["total_tokens"] == 800
        assert data["skills_info"] == "skill-a: desc"
        assert data["system_prompt"] == "You are a helpful assistant."

    def test_info_no_workspace(self, client: TestClient, mock_runtime):
        """Session 无 workspace 时 workdir 为 null；未声明展示名时为 null。"""
        mock_session = MagicMock()
        mock_session.agent.get_status.return_value = {
            "model": "gpt-4o",
            "api_url": "https://api.openai.com",
            "tools": [],
            "total_tokens": 0,
            "context_window_tokens": 128000,
            "thinking": False,
            "reasoning_effort": None,
        }
        mock_session.model_id = "gpt-4o"
        mock_session.agent.provider_name = "default"
        mock_session.agent.model_display_name = None
        mock_session.agent.yolo = False
        mock_session.session_name = None
        mock_session.session_workspace = None
        mock_session.status = "idle"
        mock_session.agent.context_manager.get_context_stats.return_value = (0, 0)
        mock_session.agent.context_manager.get_skills_info.return_value = ""
        mock_session.agent.context_manager.system_prompt.content = ""
        mock_runtime.get_session.return_value = mock_session
        mock_session.tag_meta = {}

        resp = client.get("/api/session/info", params={"session_id": "test-id"})
        assert resp.status_code == 200
        assert resp.json()["workdir"] is None
        assert resp.json()["model_display_name"] is None

    def test_info_includes_status(self, client: TestClient, mock_runtime):
        """info 响应携带运行时 status（如 waiting）。"""
        mock_session = MagicMock()
        mock_session.agent.get_status.return_value = {
            "model": "gpt-4o",
            "api_url": "https://api.openai.com",
            "tools": [],
            "total_tokens": 0,
            "context_window_tokens": 128000,
            "thinking": False,
            "reasoning_effort": None,
        }
        mock_session.model_id = "gpt-4o"
        mock_session.agent.provider_name = "default"
        mock_session.agent.model_display_name = None
        mock_session.agent.yolo = False
        mock_session.session_name = None
        mock_session.session_workspace = None
        mock_session.status = "waiting"
        mock_session.agent.context_manager.get_context_stats.return_value = (0, 0)
        mock_session.agent.context_manager.get_skills_info.return_value = ""
        mock_session.agent.context_manager.system_prompt.content = ""
        mock_runtime.get_session.return_value = mock_session
        mock_session.tag_meta = {}

        resp = client.get("/api/session/info", params={"session_id": "test-id"})
        assert resp.status_code == 200
        assert resp.json()["status"] == "waiting"

    def test_info_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.get_session.return_value = None
        resp = client.get("/api/session/info", params={"session_id": "xxx"})
        assert resp.status_code == 404


# ============================================================
# Session Branches
# ============================================================


class TestSessionBranches:
    """GET /api/session/branches 测试。"""

    def test_branches_ok(self, client: TestClient, mock_runtime):
        """正常获取分支节点列表。"""
        mock_session = MagicMock()
        mock_session.agent.context_manager.get_branch_targets.return_value = [
            {"uuid": "msg-1", "content": "hello", "role": "user"},
            {"uuid": "msg-2", "content": "world", "role": "user"},
        ]
        mock_runtime.get_session.return_value = mock_session

        resp = client.get("/api/session/branches", params={"session_id": "test-id"})
        assert resp.status_code == 200
        data = resp.json()
        assert len(data["targets"]) == 2
        assert data["targets"][0]["uuid"] == "msg-1"
        assert data["targets"][0]["content"] == "hello"

    def test_branches_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.get_session.return_value = None
        resp = client.get("/api/session/branches", params={"session_id": "xxx"})
        assert resp.status_code == 404


# ============================================================
# Session Update
# ============================================================


class TestSessionUpdate:
    """POST /api/session/update 测试。"""

    def test_update_model_id(self, client: TestClient, mock_runtime):
        """按 model_id 切换模型（provider 随映射而来，不再单独下发）。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "model_id": "gpt-4o-mini"},
        )
        assert resp.status_code == 200
        assert resp.json()["ok"] is True
        mock_runtime.update_session.assert_called_once_with(
            session_id="test-id",
            model_id="gpt-4o-mini",
            agent=None,
            title=None,
            thinking=None,
            reasoning_effort=None,
            yolo=None,
            workspace=None,
            tools=None,
        )

    def test_update_agent(self, client: TestClient, mock_runtime):
        """切换 agent 模板。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "agent": "coder"},
        )
        assert resp.status_code == 200

    def test_update_agent_not_found(self, client: TestClient, mock_runtime):
        """模板不存在返回 404。"""
        mock_runtime.update_session = AsyncMock(
            side_effect=LookupError("template 'nonexistent' not found")
        )
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "agent": "nonexistent"},
        )
        assert resp.status_code == 404
        assert "nonexistent" in resp.json()["detail"]

    def test_error_response_shape(self, client: TestClient, mock_runtime):
        """错误响应统一为 ErrorResponse 形状（error + detail）。

        回归：FastAPI 默认只返回 {"detail": ...}，缺少 wing-api-client
        期望的 error 字段，导致结构化错误反序列化恒为 None。
        """
        mock_runtime.update_session = AsyncMock(
            side_effect=LookupError("template 'nonexistent' not found")
        )
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "agent": "nonexistent"},
        )
        assert resp.status_code == 404
        body = resp.json()
        assert body["error"] == "not_found"
        assert "nonexistent" in body["detail"]

    def test_validation_error_shape(self, client: TestClient):
        """请求体校验失败（422）也输出 ErrorResponse 形状。"""
        resp = client.post("/api/session/resume", json={})
        assert resp.status_code == 422
        body = resp.json()
        assert body["error"] == "validation_error"
        assert body["detail"]

    def test_method_not_allowed_shape(self, client: TestClient):
        """405（starlette 父类 HTTPException）也输出 ErrorResponse 形状。

        /api/health 免鉴权且仅允许 GET，POST 触发 router 层 405——验证
        handler 注册在 starlette HTTPException 基类上确实覆盖了父类异常。
        """
        resp = client.post("/api/health")
        assert resp.status_code == 405
        assert resp.json()["error"] == "method_not_allowed"

    def test_update_title(self, client: TestClient, mock_runtime):
        """设置 session 名称。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "title": "my session"},
        )
        assert resp.status_code == 200

    def test_update_thinking(self, client: TestClient, mock_runtime):
        """开关 thinking 模式。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "thinking": True},
        )
        assert resp.status_code == 200

    def test_update_reasoning_effort(self, client: TestClient, mock_runtime):
        """设置推理力度。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "reasoning_effort": "high"},
        )
        assert resp.status_code == 200

    def test_update_yolo(self, client: TestClient, mock_runtime):
        """开关 yolo 模式。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "yolo": True},
        )
        assert resp.status_code == 200

    def test_update_multi_fields(self, client: TestClient, mock_runtime):
        """多字段同时更新。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={
                "session_id": "test-id",
                "agent": "coder",
                "model_id": "gpt-4o-mini",
                "title": "new title",
                "thinking": True,
                "yolo": True,
            },
        )
        assert resp.status_code == 200
        mock_runtime.update_session.assert_awaited_once()
        assert mock_runtime.update_session.call_args.kwargs["model_id"] == "gpt-4o-mini"

    def test_update_all_none(self, client: TestClient, mock_runtime):
        """所有可选字段均为 None 返回 400。"""
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id"},
        )
        assert resp.status_code == 400

    def test_update_legacy_fields_silently_ignored(
        self, client: TestClient, mock_runtime
    ):
        """旧字段 {model, provider} 被静默忽略：其它字段照常生效，不报错、不引导。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={
                "session_id": "test-id",
                "model": "gpt-4o",
                "provider": "default",
                "title": "renamed",
            },
        )
        assert resp.status_code == 200
        kwargs = mock_runtime.update_session.call_args.kwargs
        assert kwargs["model_id"] is None
        assert kwargs["title"] == "renamed"
        assert "model" not in kwargs and "provider" not in kwargs

    def test_update_legacy_only_is_no_update_field(
        self, client: TestClient, mock_runtime
    ):
        """纯旧字段请求体 = 没有任何更新字段 → 400（与全 None 同一规则）。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "model": "gpt-4o", "provider": "default"},
        )
        assert resp.status_code == 400
        assert "at least one update field" in resp.json()["detail"]
        mock_runtime.update_session.assert_not_awaited()

    def test_update_unknown_model_id_is_400(self, client: TestClient, mock_runtime):
        """未知 model_id：ValueError（C7 文案）→ 400，错误信息自解释。"""
        mock_runtime.update_session = AsyncMock(
            side_effect=ValueError("unknown model id 'sonnet'; available ids: ds-flash")
        )
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "model_id": "sonnet"},
        )
        assert resp.status_code == 400
        assert "unknown model id 'sonnet'" in resp.json()["detail"]

    def test_update_session_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.update_session = AsyncMock(
            side_effect=LookupError("session not found")
        )
        resp = client.post(
            "/api/session/update",
            json={"session_id": "xxx", "model_id": "gpt-4o"},
        )
        assert resp.status_code == 404

    def test_update_workspace(self, client: TestClient, mock_runtime):
        """切换工作目录。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "workspace": "/tmp"},
        )
        assert resp.status_code == 200
        assert resp.json()["ok"] is True
        mock_runtime.update_session.assert_called_once_with(
            session_id="test-id",
            model_id=None,
            agent=None,
            title=None,
            thinking=None,
            reasoning_effort=None,
            yolo=None,
            workspace="/tmp",
            tools=None,
        )

    def test_update_workspace_invalid(self, client: TestClient, mock_runtime):
        """workspace 路径不合法返回 400。"""
        mock_runtime.update_session = AsyncMock(
            side_effect=ValueError("workspace path does not exist: /nonexistent")
        )
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "workspace": "/nonexistent"},
        )
        assert resp.status_code == 400
        assert "does not exist" in resp.json()["detail"]


# ============================================================
# 6.3 Session Tag
# ============================================================


class TestSessionTag:
    """POST /api/session/tag 测试（读 / 写同一端点）。"""

    def test_read_without_ops(self, client: TestClient, mock_runtime):
        """add / remove 皆缺省 = 纯读：runtime 收到空元组，响应回显当前标签与记录。"""
        mock_runtime.set_session_tags.return_value = TagMutation(
            tags=["favorite"],
            added=[],
            removed=[],
            tag_meta={"favorite": TagMeta(added_at="2026-10-05T21:30:12")},
        )
        resp = client.post("/api/session/tag", json={"session_id": "test-id"})
        assert resp.status_code == 200
        assert resp.json() == {
            "ok": True,
            "session_id": "test-id",
            "tags": ["favorite"],
            "added": [],
            "removed": [],
            "tag_meta": {"favorite": {"added_at": "2026-10-05T21:30:12"}},
        }
        mock_runtime.set_session_tags.assert_called_once_with(
            "test-id", add=(), remove=()
        )

    def test_mutation_passthrough(self, client: TestClient, mock_runtime):
        """add / remove 透传；响应携带变更后的全量标签、实际增删与打标时间。"""
        mock_runtime.set_session_tags.return_value = TagMutation(
            tags=["a", "c"],
            added=["c"],
            removed=["b"],
            tag_meta={"c": TagMeta(added_at="2026-10-05T21:30:12")},
        )
        resp = client.post(
            "/api/session/tag",
            json={"session_id": "test-id", "add": ["c"], "remove": ["b"]},
        )
        assert resp.status_code == 200
        assert resp.json()["tags"] == ["a", "c"]
        assert resp.json()["added"] == ["c"]
        assert resp.json()["removed"] == ["b"]
        assert resp.json()["tag_meta"] == {"c": {"added_at": "2026-10-05T21:30:12"}}
        mock_runtime.set_session_tags.assert_called_once_with(
            "test-id", add=["c"], remove=["b"]
        )

    def test_not_found_maps_to_404(self, client: TestClient, mock_runtime):
        mock_runtime.set_session_tags.side_effect = LookupError(
            "Session not found: nope"
        )
        resp = client.post("/api/session/tag", json={"session_id": "nope"})
        assert resp.status_code == 404
        assert "nope" in resp.json()["detail"]

    def test_invalid_tag_maps_to_400(self, client: TestClient, mock_runtime):
        mock_runtime.set_session_tags.side_effect = ValueError("invalid tag: 'bad tag'")
        resp = client.post(
            "/api/session/tag", json={"session_id": "test-id", "add": ["bad tag"]}
        )
        assert resp.status_code == 400
        assert "bad tag" in resp.json()["detail"]


# ============================================================
# System: Commands
# ============================================================


class TestSystemCommands:
    """GET /api/commands 测试。"""

    @patch("wing.gateway.routes.system.magic_registry")
    def test_list_commands(self, mock_registry, client: TestClient):
        """只返回 prompt 类型的命令。"""
        prompt_cmd = MagicMock()
        prompt_cmd.name = "plan"
        prompt_cmd.aliases = []
        prompt_cmd.description = "Create a plan"
        prompt_cmd.params = "[args]"
        prompt_cmd.source = "prompt"

        builtin_cmd = MagicMock()
        builtin_cmd.name = "compact"
        builtin_cmd.aliases = []
        builtin_cmd.description = "Compact"
        builtin_cmd.params = ""
        builtin_cmd.source = "builtin"

        mock_registry.list_all.return_value = [prompt_cmd, builtin_cmd]

        resp = client.get("/api/commands")
        assert resp.status_code == 200
        data = resp.json()
        assert len(data["commands"]) == 1
        assert data["commands"][0]["name"] == "plan"


# ============================================================
# System: Models
# ============================================================


class TestSystemModels:
    """GET /api/models 测试——嵌套响应，经 runtime 取目录（路由不读 config）。

    目录是配置声明的静态投影（无远端发现）：域类型 ``ModelGroup`` / ``ModelRef``
    → wire 的 **对象数组**；``id`` 是全局唯一引用词。
    """

    def test_list_models_ok(self, client: TestClient, mock_runtime):
        """正常获取目录（按 provider 分组嵌套，每项携带 id / name / 展示元信息）。"""
        from wing.config import ModelGroup, ModelRef, ModelSpec

        mock_runtime.list_models = MagicMock(
            return_value=[
                ModelGroup(
                    provider="default",
                    models=[
                        ModelRef(
                            id="gpt-4o",
                            name="gpt-4o",
                            provider_name="default",
                            spec=ModelSpec(
                                name="gpt-4o",
                                display_name="GPT-4o",
                                description="flagship",
                                capabilities={"vision": True},  # ty: ignore[invalid-argument-type]
                            ),
                        ),
                        ModelRef(
                            id="gpt-4o-mini",
                            name="gpt-4o-mini",
                            provider_name="default",
                            spec=ModelSpec(
                                name="gpt-4o-mini", display_name="GPT-4o mini"
                            ),
                        ),
                    ],
                ),
                ModelGroup(
                    provider="claude",
                    models=[
                        ModelRef(
                            id="sonnet",
                            name="claude-opus-4",
                            provider_name="claude",
                            spec=ModelSpec(id="sonnet", name="claude-opus-4"),
                        )
                    ],
                ),
            ]
        )

        resp = client.get("/api/models")
        assert resp.status_code == 200
        assert resp.json() == {
            "providers": [
                {
                    "provider": "default",
                    "models": [
                        {
                            "id": "gpt-4o",
                            "name": "gpt-4o",
                            "display_name": "GPT-4o",
                            "description": "flagship",
                            "capabilities": {"vision": True},
                        },
                        {
                            "id": "gpt-4o-mini",
                            "name": "gpt-4o-mini",
                            "display_name": "GPT-4o mini",
                            "description": None,
                            "capabilities": {"vision": False},
                        },
                    ],
                },
                {
                    "provider": "claude",
                    "models": [
                        {
                            "id": "sonnet",
                            "name": "claude-opus-4",
                            "display_name": None,
                            "description": None,
                            "capabilities": {"vision": False},
                        }
                    ],
                },
            ]
        }

    def test_list_models_empty(self, client: TestClient, mock_runtime):
        """无 provider 时返回空分组列表。"""
        mock_runtime.list_models = MagicMock(return_value=[])

        resp = client.get("/api/models")
        assert resp.status_code == 200
        assert resp.json() == {"providers": []}


class TestModelsCatalogFromConfig:
    """真 runtime + 真 config：目录投影端到端（配置声明序 → wire，形状即契约）。

    mock 掉的 runtime 只能证明 route 的胶水；这一组证明「目录 = 配置声明的静态
    投影」——没有远端请求、没有平行数组、id 就是配置里写的那个。
    """

    @pytest.fixture
    def catalog_client(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
        from wing.config import AgentConfig, Config, ModelSpec, ProviderConfig
        from wing.gateway.server import GatewayServer
        from wing.runtime import WingRuntime

        config = Config(
            providers=[
                ProviderConfig(
                    name="local",
                    base_url="http://localhost:1/v1",
                    api_key="k",
                    models=[
                        "dfmodel",
                        ModelSpec(
                            name="dfmodel-2026",
                            display_name="DeepSeek-Flash",
                            capabilities={"vision": True},  # ty: ignore[invalid-argument-type]
                        ),
                        ModelSpec(id="ds-flash", name="sonnet", display_name="Sonnet"),
                    ],
                ),
            ],
            agents=[AgentConfig(name="default", model="dfmodel")],
        )
        # 单例替换（与 conftest 的 _mock_config 同口径）：runtime 构造与路由都读它。
        monkeypatch.setattr("wing.config.loader._config", config)
        monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))

        with patch("wing.gateway.server.load_config") as mock_load_config:
            mock_load_config.return_value = _mock_config()
            server = GatewayServer()
        server.runtime = WingRuntime()
        with TestClient(server._app) as tc:
            yield tc

    def test_catalog_follows_declaration_order_and_ids(self, catalog_client):
        resp = catalog_client.get("/api/models")
        assert resp.status_code == 200
        assert resp.json() == {
            "providers": [
                {
                    "provider": "local",
                    "models": [
                        {
                            "id": "dfmodel",
                            "name": "dfmodel",
                            "display_name": None,
                            "description": None,
                            "capabilities": {"vision": False},
                        },
                        {
                            "id": "dfmodel-2026",
                            "name": "dfmodel-2026",
                            "display_name": "DeepSeek-Flash",
                            "description": None,
                            "capabilities": {"vision": True},
                        },
                        {
                            "id": "ds-flash",
                            "name": "sonnet",
                            "display_name": "Sonnet",
                            "description": None,
                            "capabilities": {"vision": False},
                        },
                    ],
                }
            ]
        }

    def test_catalog_builds_no_provider_instances(self, catalog_client):
        """目录是纯配置投影：列目录不构造任何 provider 实例（旧实现经池懒建并发请求）。"""
        import wing.provider.pool as pool_mod

        resp = catalog_client.get("/api/models")
        assert resp.status_code == 200
        assert [m["id"] for m in resp.json()["providers"][0]["models"]] == [
            "dfmodel",
            "dfmodel-2026",
            "ds-flash",
        ]
        assert pool_mod._pool._providers == {}


# ============================================================
# System: Agents
# ============================================================


class TestSystemAgents:
    """GET /api/agents 测试。"""

    def test_list_agents(self, client: TestClient, mock_runtime):
        """正常获取 agent 模板列表。"""
        mock_runtime.template_manager.all_names = ["default", "coder", "reviewer"]
        mock_runtime.template_manager.default_name = "default"

        resp = client.get("/api/agents")
        assert resp.status_code == 200
        data = resp.json()
        assert data["agents"] == ["default", "coder", "reviewer"]
        assert data["default_agent"] == "default"


# ============================================================
# Session: Compact
# ============================================================


class TestSessionCompact:
    """POST /api/session/compact 测试。"""

    def test_compact_ok(self, client: TestClient, mock_runtime):
        """正常压缩。"""
        mock_runtime.compact_session = AsyncMock(return_value=(1000, 200))

        resp = client.post("/api/session/compact", json={"session_id": "test-id"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert data["original_tokens"] == 1000
        assert data["compressed_tokens"] == 200

    def test_compact_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.compact_session = AsyncMock(side_effect=LookupError("not found"))
        resp = client.post("/api/session/compact", json={"session_id": "xxx"})
        assert resp.status_code == 404

    def test_compact_no_compactor(self, client: TestClient, mock_runtime):
        """未配置 compactor 返回 400。"""
        mock_runtime.compact_session = AsyncMock(
            side_effect=RuntimeError("compactor not configured")
        )
        resp = client.post("/api/session/compact", json={"session_id": "test-id"})
        assert resp.status_code == 400

    def test_compact_forwards_instruction(self, client: TestClient, mock_runtime):
        """instruction 随请求体透传给 runtime.compact_session。"""
        mock_runtime.compact_session = AsyncMock(return_value=(1000, 200))

        resp = client.post(
            "/api/session/compact",
            json={
                "session_id": "test-id",
                "instruction": "保留架构决策与未完成的 TODO",
            },
        )
        assert resp.status_code == 200
        mock_runtime.compact_session.assert_called_once_with(
            "test-id", "保留架构决策与未完成的 TODO"
        )

    def test_compact_without_instruction_passes_none(
        self, client: TestClient, mock_runtime
    ):
        """缺省 instruction → None（默认压缩策略）。"""
        mock_runtime.compact_session = AsyncMock(return_value=(1000, 200))

        resp = client.post("/api/session/compact", json={"session_id": "test-id"})
        assert resp.status_code == 200
        mock_runtime.compact_session.assert_called_once_with("test-id", None)


# ============================================================
# Session: Interrupt
# ============================================================


class TestSessionInterrupt:
    """POST /api/session/interrupt 测试。"""

    def test_interrupt_ok(self, client: TestClient, mock_runtime):
        """正常中断。"""
        mock_runtime.interrupt_session = AsyncMock(return_value=None)

        resp = client.post("/api/session/interrupt", json={"session_id": "test-id"})
        assert resp.status_code == 200
        assert resp.json()["ok"] is True
        mock_runtime.interrupt_session.assert_called_once_with("test-id")

    def test_interrupt_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.interrupt_session = AsyncMock(side_effect=LookupError("not found"))
        resp = client.post("/api/session/interrupt", json={"session_id": "xxx"})
        assert resp.status_code == 404


# ============================================================
# Session: Rewind
# ============================================================


class TestSessionRewind:
    """POST /api/session/rewind 测试。"""

    def test_rewind_ok(self, client: TestClient, mock_runtime):
        """正常回退。"""
        mock_runtime.rewind_session.return_value = "draft text"

        resp = client.post(
            "/api/session/rewind",
            json={"session_id": "test-id", "target_uuid": "abc123"},
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert data["draft"] == "draft text"

    def test_rewind_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.rewind_session.side_effect = LookupError("session not found")
        resp = client.post(
            "/api/session/rewind",
            json={"session_id": "xxx", "target_uuid": "abc"},
        )
        assert resp.status_code == 404

    def test_rewind_invalid_uuid(self, client: TestClient, mock_runtime):
        """无效 uuid 返回 400。"""
        mock_runtime.rewind_session.side_effect = ValueError("invalid target uuid")
        resp = client.post(
            "/api/session/rewind",
            json={"session_id": "test-id", "target_uuid": "bad"},
        )
        assert resp.status_code == 400


# ============================================================
# System: Reload
# ============================================================


class TestSystemReload:
    """POST /api/system/reload 测试。"""

    def test_reload_ok(self, client: TestClient, mock_runtime):
        """全部重载成功。"""
        from wing.system import ReloadResult, ReloadResultItem

        mock_runtime.reload_system = AsyncMock(
            return_value=ReloadResult(
                ok=True,
                items=[
                    ReloadResultItem(name="config.yaml", ok=True),
                    ReloadResultItem(name="hooks", ok=True),
                    ReloadResultItem(name="prompt commands", ok=True),
                    ReloadResultItem(name="provider", ok=True, detail="unchanged"),
                    ReloadResultItem(name="skills & rules", ok=True),
                ],
            )
        )

        resp = client.post("/api/system/reload")
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert len(data["results"]) == 5

    def test_reload_config_failure(self, client: TestClient, mock_runtime):
        """config 加载失败立即中止。"""
        from wing.system import ReloadResult, ReloadResultItem

        mock_runtime.reload_system = AsyncMock(
            return_value=ReloadResult(
                ok=False,
                items=[
                    ReloadResultItem(name="config.yaml", ok=False, detail="bad config")
                ],
            )
        )

        resp = client.post("/api/system/reload")
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is False
        assert len(data["results"]) == 1
        assert data["results"][0]["name"] == "config.yaml"
        assert data["results"][0]["ok"] is False


# ============================================================
# Auth 鉴权测试
# ============================================================


@pytest.fixture
def auth_client(mock_runtime):
    """创建开启鉴权的 TestClient（两个 key：test-key-1, test-key-2）。"""
    with (
        patch("wing.gateway.server.WingRuntime") as MockRuntime,
        patch("wing.gateway.server.load_config") as mock_load_config,
    ):
        MockRuntime.return_value = mock_runtime
        mock_load_config.return_value = _mock_config(
            auth_enabled=True,
            auth_keys=[
                ApiKeyEntry(key="test-key-1", role="admin"),
                ApiKeyEntry(key="test-key-2", role="admin"),
            ],
        )

        from wing.gateway.server import GatewayServer

        server = GatewayServer()
        server.runtime = mock_runtime

        app = server._app
        with TestClient(app) as tc:
            yield tc


class TestAuthDisabled:
    """auth.enabled=false 时请求透传。"""

    def test_no_key_passes(self, client: TestClient):
        """鉴权关闭时，无 key 请求正常通过。"""
        resp = client.get("/api/session/list")
        assert resp.status_code == 200


class TestAuthEnabled:
    """auth.enabled=true 时的鉴权行为。"""

    def test_no_key_rejected(self, auth_client: TestClient):
        """无 key → 401。"""
        resp = auth_client.get("/api/session/list")
        assert resp.status_code == 401
        body = resp.json()
        assert body["error"] == "unauthorized"
        assert body["detail"] == "Invalid or missing API key"

    def test_wrong_key_rejected(self, auth_client: TestClient):
        """错误 key → 401。"""
        resp = auth_client.get(
            "/api/session/list",
            headers={"Authorization": "Bearer wrong-key"},
        )
        assert resp.status_code == 401

    def test_bearer_header_ok(self, auth_client: TestClient):
        """Bearer header 正确 key → 200。"""
        resp = auth_client.get(
            "/api/session/list",
            headers={"Authorization": "Bearer test-key-1"},
        )
        assert resp.status_code == 200

    def test_x_api_key_header_ok(self, auth_client: TestClient):
        """X-API-Key header 正确 key → 200。"""
        resp = auth_client.get(
            "/api/session/list",
            headers={"X-API-Key": "test-key-1"},
        )
        assert resp.status_code == 200

    def test_health_exempt(self, auth_client: TestClient):
        """/api/health 免鉴权。"""
        resp = auth_client.get("/api/health")
        assert resp.status_code == 200
        assert resp.json()["status"] == "ok"

    def test_second_key_ok(self, auth_client: TestClient):
        """多 key 配置，使用第二个 key 通过鉴权。"""
        resp = auth_client.get(
            "/api/session/list",
            headers={"Authorization": "Bearer test-key-2"},
        )
        assert resp.status_code == 200

    def test_post_endpoint_auth(self, auth_client: TestClient):
        """POST 端点同样需要鉴权。"""
        resp = auth_client.post("/api/session/create", json={})
        assert resp.status_code == 401

        resp = auth_client.post(
            "/api/session/create",
            json={},
            headers={"Authorization": "Bearer test-key-1"},
        )
        assert resp.status_code == 200


class TestAuthConfigVerify:
    """AuthConfig.verify() 单元测试。"""

    def test_correct_key_returns_role(self):
        cfg = AuthConfig(enabled=True, keys=[ApiKeyEntry(key="abc", role="admin")])
        assert cfg.verify("abc") == "admin"

    def test_wrong_key_returns_none(self):
        cfg = AuthConfig(enabled=True, keys=[ApiKeyEntry(key="abc")])
        assert cfg.verify("xyz") is None

    def test_unicode_key_rejected(self):
        """非 ASCII key 在配置解析时即被拒绝。"""
        with pytest.raises(Exception):
            AuthConfig(enabled=True, keys=[ApiKeyEntry(key="🌙月亮")])

    def test_empty_keys(self):
        cfg = AuthConfig(enabled=True, keys=[])
        assert cfg.verify("anything") is None

    def test_custom_role(self):
        cfg = AuthConfig(
            enabled=True,
            keys=[ApiKeyEntry(key="tool-key", role="tool_runtime")],
        )
        assert cfg.verify("tool-key") == "tool_runtime"


class TestWsAuth:
    """WebSocket 连接鉴权测试。"""

    def test_ws_bearer_header_ok(self, auth_client: TestClient):
        """WS + Bearer header 正确 key → 连接成功，收到 ConnectResponse。"""
        with auth_client.websocket_connect(
            "/ws", headers={"Authorization": "Bearer test-key-1"}
        ) as ws:
            data = ws.receive_json()
            assert data["type"] == "connected"
            assert "client_id" in data

    def test_ws_query_param_ok(self, auth_client: TestClient):
        """WS + query param api_key → 连接成功。"""
        with auth_client.websocket_connect("/ws?api_key=test-key-1") as ws:
            data = ws.receive_json()
            assert data["type"] == "connected"

    def test_ws_no_key_rejected(self, auth_client: TestClient):
        """WS 无 key → 连接被拒绝。"""
        with pytest.raises(Exception):
            with auth_client.websocket_connect("/ws"):
                pass

    def test_ws_wrong_key_rejected(self, auth_client: TestClient):
        """WS 错误 key → 连接被拒绝。"""
        with pytest.raises(Exception):
            with auth_client.websocket_connect(
                "/ws", headers={"Authorization": "Bearer wrong"}
            ):
                pass

    def test_ws_auth_disabled_no_key(self, client: TestClient):
        """鉴权关闭时，WS 无 key 正常连接。"""
        with client.websocket_connect("/ws") as ws:
            data = ws.receive_json()
            assert data["type"] == "connected"


class TestAuthHotReload:
    """auth 配置热重载测试——property 读最新单例，不缓存。"""

    def test_reload_toggles_auth(self, mock_runtime):
        """运行时切换 auth.enabled，无需重启即生效。"""
        mock_cfg = _mock_config(auth_enabled=False)
        with (
            patch("wing.gateway.server.WingRuntime") as MockRuntime,
            patch("wing.gateway.server.load_config") as mock_load_config,
        ):
            MockRuntime.return_value = mock_runtime
            mock_load_config.return_value = mock_cfg

            from wing.gateway.server import GatewayServer

            server = GatewayServer()
            server.runtime = mock_runtime

            with TestClient(server._app) as tc:
                # auth disabled → 200
                resp = tc.get("/api/session/list")
                assert resp.status_code == 200

                # 模拟热重载：切换到 auth enabled
                mock_load_config.return_value = _mock_config(
                    auth_enabled=True,
                    auth_keys=[ApiKeyEntry(key="new-key", role="admin")],
                )

                # 无 key → 401（立即生效，无需重启）
                resp = tc.get("/api/session/list")
                assert resp.status_code == 401

                # 正确 key → 200
                resp = tc.get(
                    "/api/session/list",
                    headers={"Authorization": "Bearer new-key"},
                )
                assert resp.status_code == 200


# ============================================================
# RBAC 角色强制测试（admin / tool_runtime）
# ============================================================


@pytest.fixture
def rbac_env(mock_runtime):
    """开启鉴权 + 两种角色的环境：admin-key(admin)、tool-key(tool_runtime)。

    yield (server, client)——暴露 server 以便操作 remote_tools / clients。
    """
    with (
        patch("wing.gateway.server.WingRuntime") as MockRuntime,
        patch("wing.gateway.server.load_config") as mock_load_config,
    ):
        MockRuntime.return_value = mock_runtime
        mock_load_config.return_value = _mock_config(
            auth_enabled=True,
            auth_keys=[
                ApiKeyEntry(key="admin-key", role="admin"),
                ApiKeyEntry(key="tool-key", role="tool_runtime"),
            ],
        )

        from wing.gateway.server import GatewayServer

        server = GatewayServer()
        server.runtime = mock_runtime

        with TestClient(server._app) as tc:
            yield server, tc


TOOL_HEADERS = {"Authorization": "Bearer tool-key"}
ADMIN_HEADERS = {"Authorization": "Bearer admin-key"}


class TestRBAC:
    """tool_runtime 受限、admin 全量。"""

    def test_tool_runtime_restricted_endpoint_403(self, rbac_env):
        """tool_runtime 访问工具注册以外的端点 → 403 + ErrorResponse 形状。"""
        _, tc = rbac_env
        resp = tc.get("/api/session/list", headers=TOOL_HEADERS)
        assert resp.status_code == 403
        body = resp.json()
        assert body["error"] == "forbidden"
        assert "detail" in body

    def test_tool_runtime_create_session_403(self, rbac_env):
        """tool_runtime 创建 session → 403。"""
        _, tc = rbac_env
        resp = tc.post("/api/session/create", json={}, headers=TOOL_HEADERS)
        assert resp.status_code == 403

    def test_tool_runtime_register_allowed_through_rbac(self, rbac_env):
        """tool_runtime 访问注册端点不被 RBAC 拦（到达路由，因未连接 → 400）。"""
        _, tc = rbac_env
        resp = tc.post(
            "/api/tools/register",
            json={"tools": []},
            headers={**TOOL_HEADERS, "X-Client-Id": "ghost"},
        )
        # 400（无活跃 WS）而非 403/401——证明 RBAC allowlist 放行
        assert resp.status_code == 400
        assert "WebSocket" in resp.json()["detail"]

    def test_tool_runtime_register_missing_client_id_header(self, rbac_env):
        """注册端点缺 X-Client-Id → 400（仍非 403）。"""
        _, tc = rbac_env
        resp = tc.post("/api/tools/register", json={"tools": []}, headers=TOOL_HEADERS)
        assert resp.status_code == 400

    def test_admin_full_access(self, rbac_env):
        """admin 访问受限端点 → 200。"""
        _, tc = rbac_env
        resp = tc.get("/api/session/list", headers=ADMIN_HEADERS)
        assert resp.status_code == 200

    def test_admin_can_reach_register(self, rbac_env):
        """admin 也能到达注册端点（全量权限）。"""
        _, tc = rbac_env
        resp = tc.post(
            "/api/tools/register",
            json={"tools": []},
            headers={**ADMIN_HEADERS, "X-Client-Id": "ghost"},
        )
        assert resp.status_code == 400  # 到达路由，未连接 → 400

    def test_register_success_with_attached_client(self, rbac_env):
        """已连接 client 注册工具成功，落入核心 registry（namespace=client_id）。"""
        from wing.tool_registry import tool_registry

        server, tc = rbac_env
        server.remote_tools.attach("host-1", MagicMock())
        try:
            resp = tc.post(
                "/api/tools/register",
                json={"tools": [{"name": "Read", "description": "d", "params": []}]},
                headers={**TOOL_HEADERS, "X-Client-Id": "host-1"},
            )
            assert resp.status_code == 200
            body = resp.json()
            assert body["ok"] is True
            assert body["registered"] == ["host-1.Read"]
            assert tool_registry.resolve("host-1.Read") is not None
        finally:
            tool_registry.unregister_namespace("host-1")


# ============================================================
# 远程工具 WS 流程测试（tool host 连接 / 断连）
# ============================================================


class TestRemoteToolWS:
    """tool_runtime 经 WS 声明 client_id、断连注销工具。"""

    def test_tool_host_connect_declares_client_id(self, rbac_env):
        """tool host 连接时自选 client_id，连接成功并被 attach。"""
        server, tc = rbac_env
        with tc.websocket_connect("/ws?client_id=host-ws", headers=TOOL_HEADERS) as ws:
            data = ws.receive_json()
            assert data["type"] == "connected"
            assert data["client_id"] == "host-ws"
            assert server.remote_tools.is_attached("host-ws")

    def test_tool_host_connect_requires_client_id(self, rbac_env):
        """tool host 未声明 client_id → 拒连。"""
        _, tc = rbac_env
        with pytest.raises(Exception):
            with tc.websocket_connect("/ws", headers=TOOL_HEADERS):
                pass

    def test_tool_host_client_id_collision(self, rbac_env):
        """client_id 冲突 → 第二个连接被拒。"""
        _, tc = rbac_env
        with tc.websocket_connect("/ws?client_id=dup", headers=TOOL_HEADERS):
            with pytest.raises(Exception):
                with tc.websocket_connect("/ws?client_id=dup", headers=TOOL_HEADERS):
                    pass

    def test_admin_ws_still_server_assigned(self, rbac_env):
        """admin WS 维持服务端分配 client_id（既有行为不变）。"""
        _, tc = rbac_env
        with tc.websocket_connect("/ws", headers=ADMIN_HEADERS) as ws:
            data = ws.receive_json()
            assert data["type"] == "connected"
            assert data["client_id"]  # 服务端分配的非空 id

    def test_tool_host_disconnect_unregisters_tools(self, rbac_env):
        """tool host 断连后其远程工具被注销。"""
        import time

        from wing.tool_registry import tool_registry

        server, tc = rbac_env
        with tc.websocket_connect("/ws?client_id=host-dc", headers=TOOL_HEADERS) as ws:
            ws.receive_json()  # connected
            resp = tc.post(
                "/api/tools/register",
                json={"tools": [{"name": "Read", "description": "d", "params": []}]},
                headers={**TOOL_HEADERS, "X-Client-Id": "host-dc"},
            )
            assert resp.status_code == 200
            assert tool_registry.resolve("host-dc.Read") is not None

        # 断连后 fail_client 注销工具（轮询等待 app 处理 close）
        deadline = time.time() + 2.0
        while (
            tool_registry.resolve("host-dc.Read") is not None and time.time() < deadline
        ):
            time.sleep(0.02)
        assert tool_registry.resolve("host-dc.Read") is None
        assert not server.remote_tools.is_attached("host-dc")


# ============================================================
# client_id 自定义与权限解耦（先来先得到，全量唯一性）
# ============================================================


class TestClientIdDecoupledFromRole:
    """client_id 自选是身份/UX 概念，与角色无关；唯一性对所有角色一致。"""

    def test_admin_can_declare_client_id(self, rbac_env):
        """admin 也可自选 client_id（既注册工具又收事件）。"""
        server, tc = rbac_env
        with tc.websocket_connect(
            "/ws?client_id=my-admin", headers=ADMIN_HEADERS
        ) as ws:
            data = ws.receive_json()
            assert data["client_id"] == "my-admin"
            assert server.remote_tools.is_attached("my-admin")
            assert server.remote_tools.receives_events("my-admin") is True

    def test_admin_client_id_collision_rejected(self, rbac_env):
        """唯一性对所有角色一致——admin 撞 id 同样被拒。"""
        _, tc = rbac_env
        with tc.websocket_connect("/ws?client_id=dup-admin", headers=ADMIN_HEADERS):
            with pytest.raises(Exception):
                with tc.websocket_connect(
                    "/ws?client_id=dup-admin", headers=ADMIN_HEADERS
                ):
                    pass

    def test_no_auth_client_can_declare_client_id(self, client: TestClient):
        """鉴权关闭时同样支持自定义 client_id（与角色完全解耦）。"""
        with client.websocket_connect("/ws?client_id=free-id") as ws:
            data = ws.receive_json()
            assert data["client_id"] == "free-id"

    def test_tool_runtime_receives_events_false(self, rbac_env):
        """tool_runtime attach 后 receives_events=False（纯执行远端）。"""
        server, tc = rbac_env
        with tc.websocket_connect("/ws?client_id=host-ev", headers=TOOL_HEADERS) as ws:
            ws.receive_json()
            assert server.remote_tools.receives_events("host-ev") is False


# ============================================================
# tool_runtime WS 边界 + 帧分流
# ============================================================


class TestToolRuntimeWsBoundary:
    """tool_runtime 是纯工具执行远端：不投递用户消息。"""

    def test_tool_runtime_cannot_send_user_message(self, rbac_env):
        """tool host 发用户消息帧 → ErrorEvent，runtime.post 不被调用。"""
        server, tc = rbac_env
        server.runtime.post = AsyncMock()
        with tc.websocket_connect("/ws?client_id=host-msg", headers=TOOL_HEADERS) as ws:
            ws.receive_json()  # connected
            ws.send_json({"session_id": "s1", "content": "inject"})
            err = ws.receive_json()
            assert err["type"] == "error"
            server.runtime.post.assert_not_called()

    def test_receives_events_predicate(self, rbac_env):
        """_receives_events：tool host 跳过、admin host 与纯前端接收。"""
        server, _ = rbac_env
        mgr = server.remote_tools
        mgr.attach("tool-host", MagicMock(), receives_events=False)
        mgr.attach("admin-host", MagicMock(), receives_events=True)
        assert server._receives_events("tool-host") is False
        assert server._receives_events("admin-host") is True
        assert server._receives_events("pure-frontend") is True  # 未 attach


class TestWsFrameRouting:
    """入站帧按 call_id 分流：结果帧 → manager，用户帧 → runtime.post。"""

    def test_call_id_frame_routes_to_manager(self, rbac_env):
        """含 call_id 帧交 manager.resolve_result（带 client 归属），不走 post。"""
        server, tc = rbac_env
        server.runtime.post = AsyncMock()
        server.remote_tools.resolve_result = MagicMock(return_value=False)
        with tc.websocket_connect(
            "/ws?client_id=host-route", headers=TOOL_HEADERS
        ) as ws:
            ws.receive_json()
            ws.send_json(
                {
                    "type": "tool_call_result",
                    "call_id": "c1",
                    "result": "r",
                    "is_error": False,
                }
            )
            # 发一个用户帧触发 ErrorEvent 以同步（WS 有序，保证前帧已处理）
            ws.send_json({"session_id": "s1", "content": "x"})
            err = ws.receive_json()
            assert err["type"] == "error"
            server.remote_tools.resolve_result.assert_called_once()
            assert server.remote_tools.resolve_result.call_args[0] == (
                "host-route",
                "c1",
                "r",
                False,
            )
            server.runtime.post.assert_not_called()

    def test_normal_frame_routes_to_post(self, rbac_env):
        """admin 的普通帧走 runtime.post，不触达 resolve_result。"""
        import time

        server, tc = rbac_env
        server.runtime.post = AsyncMock()
        server.remote_tools.resolve_result = MagicMock(return_value=False)
        with tc.websocket_connect("/ws", headers=ADMIN_HEADERS) as ws:
            ws.receive_json()  # connected
            ws.send_json({"request_id": "r1", "session_id": "s1", "content": "hi"})
            deadline = time.time() + 2.0
            while not server.runtime.post.called and time.time() < deadline:
                time.sleep(0.02)
            server.runtime.post.assert_called_once()
            server.remote_tools.resolve_result.assert_not_called()


# ============================================================
# 保留字 client_id + admin 断连注销 + llm_name 注册
# ============================================================


class TestReservedClientIdAndLifecycle:
    def test_client_id_default_reserved(self, rbac_env):
        """client_id='default' 是内置命名空间保留字，连接即拒。"""
        _, tc = rbac_env
        with pytest.raises(Exception):
            with tc.websocket_connect("/ws?client_id=default", headers=TOOL_HEADERS):
                pass

    def test_admin_declared_disconnect_unregisters_tools(self, rbac_env):
        """声明了 client_id 的 admin 断连后，其远程工具同样被注销。"""
        import time

        from wing.tool_registry import tool_registry

        server, tc = rbac_env
        with tc.websocket_connect(
            "/ws?client_id=admin-host", headers=ADMIN_HEADERS
        ) as ws:
            ws.receive_json()
            resp = tc.post(
                "/api/tools/register",
                json={"tools": [{"name": "Read", "description": "d", "params": []}]},
                headers={**ADMIN_HEADERS, "X-Client-Id": "admin-host"},
            )
            assert resp.status_code == 200
            assert tool_registry.resolve("admin-host.Read") is not None

        deadline = time.time() + 2.0
        while (
            tool_registry.resolve("admin-host.Read") is not None
            and time.time() < deadline
        ):
            time.sleep(0.02)
        assert tool_registry.resolve("admin-host.Read") is None
        assert not server.remote_tools.is_attached("admin-host")

    def test_register_with_llm_name(self, rbac_env):
        """端上声明的 llm_name 经注册端点透传到核心 Tool。"""
        from wing.tool_registry import tool_registry

        server, tc = rbac_env
        server.remote_tools.attach("host-llm", MagicMock())
        try:
            resp = tc.post(
                "/api/tools/register",
                json={
                    "tools": [
                        {
                            "name": "Read",
                            "description": "d",
                            "llm_name": "RemoteRead",
                            "params": [],
                        }
                    ]
                },
                headers={**TOOL_HEADERS, "X-Client-Id": "host-llm"},
            )
            assert resp.status_code == 200
            tool = tool_registry.resolve("host-llm.Read")
            assert tool is not None
            assert tool.effective_llm_name == "RemoteRead"
        finally:
            tool_registry.unregister_namespace("host-llm")

    def test_register_bad_llm_name_rejected(self, rbac_env):
        """坏 llm_name（含点）在注册端点被拒（422 校验错误）。"""
        from wing.tool_registry import tool_registry

        server, tc = rbac_env
        server.remote_tools.attach("host-bad", MagicMock())
        try:
            resp = tc.post(
                "/api/tools/register",
                json={
                    "tools": [
                        {
                            "name": "Read",
                            "description": "d",
                            "llm_name": "a.b",
                            "params": [],
                        }
                    ]
                },
                headers={**TOOL_HEADERS, "X-Client-Id": "host-bad"},
            )
            assert resp.status_code == 422
        finally:
            tool_registry.unregister_namespace("host-bad")


# ============================================================
# 真实 runtime 的 id 闸门（HTTP 面：400，而不是 500）
# ============================================================


class TestRealRuntimeGates:
    """非法输入在**真实** runtime 上的 HTTP 表现：400/404 + 零残留（绝不 500）。

    覆盖"孤立代理字符"这一族：``{"session_id": "\\ud800"}`` 是合法 JSON，但值不是
    合法 UTF-8——旧实现里 create 会成功、随后在响应序列化（``PydanticSerializationError``）
    或 metadata 落盘处炸 500；更新一点的路径（agent 覆盖的字符串字段）还会留下
    半份 metadata，让下一次同 id 的 create "收养"这个半成品幽灵会话。

    httpx 的 ``json=`` 在客户端就编码不了代理字符，因此这里发**原始字节体**
    （服务端收到的是转义形式，pydantic 解出代理字符）——正是恶意 / 异常客户端
    能造出的那一类请求。
    """

    @pytest.fixture
    def real_client(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
        """真实 WingRuntime + 独立 sessions 根（不与共享临时目录混）。"""
        from wing.gateway.server import GatewayServer
        from wing.runtime import WingRuntime

        monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
        with patch("wing.gateway.server.load_config") as mock_load_config:
            mock_load_config.return_value = _mock_config()
            server = GatewayServer()
        server.runtime = WingRuntime()
        with TestClient(server._app) as tc:
            yield tc, tmp_path / "sessions"

    def test_surrogate_session_id_is_400_not_500(self, real_client):
        client, sessions_root = real_client
        resp = client.post(
            "/api/session/create",
            content=b'{"session_id": "\\ud800"}',
            headers={"content-type": "application/json"},
        )
        assert resp.status_code == 400, resp.text
        assert "invalid session id" in resp.json()["detail"]
        assert not sessions_root.exists() or list(sessions_root.iterdir()) == []

    def test_surrogate_id_on_tag_is_404_not_500(self, real_client):
        """读路径：闸门把非法 id 折成"不存在"（404），且错误体可序列化。"""
        client, _ = real_client
        resp = client.post(
            "/api/session/tag",
            content=b'{"session_id": "\\ud800"}',
            headers={"content-type": "application/json"},
        )
        assert resp.status_code == 404, resp.text
        assert "Session not found" in resp.json()["detail"]

    def test_overlong_byte_id_is_400(self, real_client):
        """128 字符的 CJK（= 384 字节）在 Linux 上会让 mkdir ENAMETOOLONG：闸门先拒。"""
        client, sessions_root = real_client
        resp = client.post("/api/session/create", json={"session_id": "收" * 128})
        assert resp.status_code == 400, resp.text
        assert not sessions_root.exists() or list(sessions_root.iterdir()) == []

    # ── N1：覆盖 / 文本字段的非 UTF-8 输入（400 而非 500，零残留、无幽灵） ──

    def test_surrogate_in_override_is_400_not_500(self, real_client):
        """agent 覆盖里的孤立代理字符 → 400；**零残留**（连目录都不建）。"""
        client, sessions_root = real_client
        resp = client.post(
            "/api/session/create",
            content=(
                b'{"session_id": "Ghost-R", "agent": '
                b'{"model_id": "m", "system_prompt": "\\ud800"}}'
            ),
            headers={"content-type": "application/json"},
        )
        assert resp.status_code == 400, resp.text
        assert "system_prompt" in resp.json()["detail"], resp.text
        # 零残留：没有目录、没有 metadata——认领键发生在校验之后
        assert not sessions_root.exists() or list(sessions_root.iterdir()) == []

    def test_no_ghost_session_is_adopted_after_the_failed_create(self, real_client):
        """失败之后同 id 的 create 是**新建**，不是收养半成品幽灵。"""
        client, _ = real_client
        failed = client.post(
            "/api/session/create",
            content=(
                b'{"session_id": "Ghost-R", "agent": '
                b'{"model_id": "m", "system_prompt": "\\ud800"}}'
            ),
            headers={"content-type": "application/json"},
        )
        assert failed.status_code == 400, failed.text

        again = client.post("/api/session/create", json={"session_id": "Ghost-R"})
        assert again.status_code == 200, again.text
        assert again.json()["session_id"] == "Ghost-R"
        # 幽灵的痕迹（model_id / system_prompt 记录）不存在 → 这是全新会话。
        state = client.get("/api/session/get", params={"session_id": "Ghost-R"})
        assert state.status_code == 200, state.text
        assert state.json()["messages"] == []

    def test_surrogate_in_workspace_or_title_is_400(self, real_client):
        """同一族输入的另一半：workspace（create/update）与 title 也必须 400。"""
        client, sessions_root = real_client
        created = client.post("/api/session/create", json={"session_id": "W-1"})
        assert created.status_code == 200, created.text

        bad_workspace = client.post(
            "/api/session/create",
            content=(b'{"session_id": "W-2", "workspace": "/tmp/\\ud800"}'),
            headers={"content-type": "application/json"},
        )
        assert bad_workspace.status_code == 400, bad_workspace.text
        assert not (sessions_root / "W-2").exists()

        bad_title = client.post(
            "/api/session/update",
            content=(b'{"session_id": "W-1", "title": "\\ud800"}'),
            headers={"content-type": "application/json"},
        )
        assert bad_title.status_code == 400, bad_title.text

    def test_surrogate_in_message_content_is_400(self, real_client):
        """消息正文：非法 UTF-8 → 400（它在首条消息时还会成为标题）。"""
        client, _ = real_client
        created = client.post("/api/session/create", json={"session_id": "C-1"})
        assert created.status_code == 200, created.text
        resp = client.post(
            "/api/session/send",
            content=b'{"session_id": "C-1", "content": "\\ud800"}',
            headers={"content-type": "application/json"},
        )
        assert resp.status_code == 400, resp.text
        assert "message content" in resp.json()["detail"]

    def test_error_detail_echoing_a_surrogate_is_serialisable(self, real_client):
        """错误文案回显原始输入时也不能炸：出口统一转义（否则 400 变 500）。"""
        client, _ = real_client
        created = client.post("/api/session/create", json={"session_id": "E-1"})
        assert created.status_code == 200, created.text
        # 模板名带孤立代理字符：域内报错会**回显它**（"template '<x>' not found"），
        # 原始形式会让响应序列化炸成 500。
        resp = client.post(
            "/api/session/update",
            content=b'{"session_id": "E-1", "agent": "\\ud800"}',
            headers={"content-type": "application/json"},
        )
        assert resp.status_code == 404, resp.text
        assert "\\ud800" in resp.json()["detail"], resp.text

    # ── update 的 model_id / reasoning_effort 也必须先过闸门 ──

    def test_update_model_surrogate_is_400_without_poisoning(self, real_client):
        """毒化路径：内存态先被写脏 → `/info` 500、后续写全失败（修复前）。"""
        client, sessions_root = real_client
        created = client.post("/api/session/create", json={"session_id": "U-1"})
        assert created.status_code == 200, created.text

        bad = client.post(
            "/api/session/update",
            content=b'{"session_id": "U-1", "model_id": "\\ud800"}',
            headers={"content-type": "application/json"},
        )
        assert bad.status_code == 400, bad.text
        assert "model_id must be UTF-8 encodable" in bad.json()["detail"]

        # 未被毒化：读端点与后续写操作全部照常，metadata 无痕
        assert (
            client.get("/api/session/info", params={"session_id": "U-1"}).status_code
            == 200
        )
        assert (
            client.post(
                "/api/session/update", json={"session_id": "U-1", "title": "ok"}
            ).status_code
            == 200
        )
        assert (
            client.post(
                "/api/session/send", json={"session_id": "U-1", "content": "hi"}
            ).status_code
            == 200
        )
        metadata = (sessions_root / "U-1" / "metadata.json").read_text()
        assert "model_name" not in metadata, metadata

    def test_update_reasoning_effort_surrogate_is_400_without_poisoning(
        self, real_client
    ):
        """对照组：另一条同形路径（provider 级开关）同样是"拒绝在 mutation 之前"。"""
        client, _ = real_client
        created = client.post("/api/session/create", json={"session_id": "U-2"})
        assert created.status_code == 200, created.text

        bad = client.post(
            "/api/session/update",
            content=b'{"session_id": "U-2", "reasoning_effort": "\\ud800"}',
            headers={"content-type": "application/json"},
        )
        assert bad.status_code == 400, bad.text
        assert "reasoning_effort must be UTF-8 encodable" in bad.json()["detail"]

        assert (
            client.get("/api/session/info", params={"session_id": "U-2"}).status_code
            == 200
        )
        assert (
            client.post(
                "/api/session/update", json={"session_id": "U-2", "title": "ok"}
            ).status_code
            == 200
        )

    # ── 模型引用词：update{model_id} → info 三件套（真实 runtime 端到端） ──

    def test_update_model_id_and_info_round_trip(self, real_client):
        """真实 runtime：按 id 切换 → info 携带 (model, model_id, provider_name)。

        测试配置（conftest）：default → gpt-4；alt → qwen3-max / gpt-4o-mini /
        claude-x（id 缺省 = 调用名）。
        """
        client, sessions_root = real_client
        created = client.post("/api/session/create", json={"session_id": "M-1"})
        assert created.status_code == 200, created.text

        info = client.get("/api/session/info", params={"session_id": "M-1"})
        assert info.status_code == 200, info.text
        assert info.json()["model"] == "gpt-4"
        assert info.json()["model_id"] == "gpt-4"
        assert info.json()["provider_name"] == "default"

        updated = client.post(
            "/api/session/update",
            json={"session_id": "M-1", "model_id": "qwen3-max"},
        )
        assert updated.status_code == 200, updated.text

        after = client.get("/api/session/info", params={"session_id": "M-1"}).json()
        assert after["model"] == "qwen3-max"
        assert after["model_id"] == "qwen3-max"
        assert after["provider_name"] == "alt"

        # 三元组落盘（跨重启恢复的唯一来源）
        metadata = json.loads(
            (sessions_root / "M-1" / "metadata.json").read_text(encoding="utf-8")
        )
        assert (
            metadata["model_id"],
            metadata["model_name"],
            metadata["provider_name"],
        ) == ("qwen3-max", "qwen3-max", "alt")

    def test_unknown_model_id_is_400_with_available_ids(self, real_client):
        """未知 id：400 + available ids + name 提示（外部编排方一次改对）。"""
        client, _ = real_client
        assert (
            client.post("/api/session/create", json={"session_id": "M-2"}).status_code
            == 200
        )
        resp = client.post(
            "/api/session/update",
            json={"session_id": "M-2", "model_id": "sonnet"},
        )
        assert resp.status_code == 400, resp.text
        detail = resp.json()["detail"]
        assert "unknown model id 'sonnet'" in detail
        assert "available ids:" in detail
        assert "gpt-4" in detail  # 列出可选 id（本测试配置）
        # 会话仍在原模型上（先查后改，零变化）
        info = client.get("/api/session/info", params={"session_id": "M-2"}).json()
        assert info["model"] == "gpt-4"


class TestSendRouteErrorMapping:
    """`send` 路由的错误映射口径：**只有输入非法**才回 400。

    终轮复审 N1：把 `runtime.post` 抛出的任何 `ValueError` 都映射成 400 太宽——
    链上将来出现的内部 `ValueError`（队列关闭、prompt 命令语义错误）会被报成
    "客户端的错"。现在只认 `InvalidInputError`（文本闸门专用类型），其它
    `ValueError` 走 500（带着栈进日志）。
    """

    def test_invalid_input_error_maps_to_400(self, client, mock_runtime):
        from wing.common.utils import InvalidInputError

        mock_runtime.post = AsyncMock(
            side_effect=InvalidInputError("message content must be UTF-8 encodable")
        )
        resp = client.post(
            "/api/session/send", json={"session_id": "sid-1", "content": "hi"}
        )
        assert resp.status_code == 400, resp.text
        assert "must be UTF-8 encodable" in resp.json()["detail"]

    def test_other_value_error_is_not_blamed_on_the_client(self, client, mock_runtime):
        """内部 ValueError **不该**被折成 400。

        TestClient 默认把未捕获异常重新抛出（`raise_server_exceptions=True`），
        因此这里断言它**穿透**路由——生产里由 ASGI 层转成 500（带栈进日志），
        关键是它不会被当成"客户端的错"。
        """
        mock_runtime.post = AsyncMock(side_effect=ValueError("inbox closed"))
        with pytest.raises(ValueError, match="inbox closed"):
            client.post(
                "/api/session/send", json={"session_id": "sid-1", "content": "hi"}
            )

    def test_missing_session_is_still_404(self, client, mock_runtime):
        mock_runtime.ensure_loaded.side_effect = LookupError("nope")
        resp = client.post(
            "/api/session/send", json={"session_id": "sid-1", "content": "hi"}
        )
        assert resp.status_code == 404, resp.text
