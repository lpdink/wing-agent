"""Gateway HTTP 端点单元测试。

使用 FastAPI TestClient + Mock WingRuntime 测试所有 HTTP 路由。
不启动真实 uvicorn server，不依赖端口 32523。
"""

from __future__ import annotations

from datetime import datetime
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from wing.event import AgentInfo, SessionInfo


@pytest.fixture
def mock_runtime():
    """创建 mock WingRuntime，避免初始化真实 LLM 连接。"""
    runtime = MagicMock()

    # create_session 默认返回 mock session
    mock_session = MagicMock()
    mock_session.session_id = "test-session-id"
    mock_session.template_name = "default"
    mock_session.session_workspace = "/tmp"
    runtime.create_session.return_value = mock_session

    # resume_session 默认返回 mock session
    runtime.resume_session.return_value = mock_session

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
    # 需要 mock WingRuntime.__init__ 和 load_hooks 避免真实初始化
    with patch("wing.gateway.server.WingRuntime") as MockRuntime:
        MockRuntime.return_value = mock_runtime

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
        """健康检查返回 200 + status=ok + version。"""
        resp = client.get("/api/health")
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "ok"
        assert "version" in data


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
        mock_runtime.create_session.assert_called_once_with(
            template_name=None, workspace=None
        )

    def test_create_with_template(self, client: TestClient, mock_runtime):
        """指定模板创建 session。"""
        resp = client.post(
            "/api/session/create",
            json={"template_name": "coder", "workspace": "/ws"},
        )
        assert resp.status_code == 200
        mock_runtime.create_session.assert_called_once_with(
            template_name="coder", workspace="/ws"
        )

    def test_create_template_not_found(self, client: TestClient, mock_runtime):
        """模板不存在返回 400。"""
        mock_runtime.create_session.side_effect = ValueError("template 'xxx' not found")
        resp = client.post("/api/session/create", json={"template_name": "xxx"})
        assert resp.status_code == 400


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
        mock_runtime.resume_session.assert_called_once_with("abc123")

    def test_resume_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.resume_session.side_effect = ValueError("Session not found: xxx")
        resp = client.post("/api/session/resume", json={"session_id": "xxx"})
        assert resp.status_code == 404


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
        mock_runtime.fork_session.side_effect = ValueError(
            "Fork failed: source session 'xxx' not found or target_uuid 'yyy' invalid"
        )
        resp = client.post(
            "/api/session/fork",
            json={"source_session_id": "xxx", "target_uuid": "yyy"},
        )
        assert resp.status_code == 404

    def test_fork_uuid_not_found(self, client: TestClient, mock_runtime):
        """目标 UUID 不存在返回 404。"""
        mock_runtime.fork_session.side_effect = ValueError(
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
        mock_runtime.subscribe.side_effect = ValueError("Session not found: xxx")
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
        """Session 不存在返回 404。"""
        mock_runtime.get_session_state.return_value = None
        resp = client.post(
            "/api/session/send",
            json={"session_id": "xxx", "content": "hello"},
        )
        assert resp.status_code == 404


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
