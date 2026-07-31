"""Gateway HTTP 端点单元测试。

使用 FastAPI TestClient + Mock WingRuntime 测试所有 HTTP 路由。
不启动真实 uvicorn server，不依赖端口 32523。
"""

from __future__ import annotations

from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from wing.config import ApiKeyEntry, AuthConfig
from wing.event import AgentInfo, SessionInfo


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
        assert data["backend"] == "file"
        mock_runtime.create_session.assert_called_once_with(
            template_name=None, workspace=None, agent_override=None, backend=None
        )

    def test_create_with_template(self, client: TestClient, mock_runtime):
        """指定模板创建 session。"""
        resp = client.post(
            "/api/session/create",
            json={"template_name": "coder", "workspace": "/ws"},
        )
        assert resp.status_code == 200
        mock_runtime.create_session.assert_called_once_with(
            template_name="coder", workspace="/ws", agent_override=None, backend=None
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
            template_name=None, workspace=None, agent_override=None, backend="memory"
        )

    def test_create_unknown_backend_returns_400(self, client: TestClient, mock_runtime):
        """未知 backend 返回 400。"""
        mock_runtime.create_session.side_effect = ValueError(
            "Unknown storage backend 'redis'. Available: ['file', 'memory']"
        )
        resp = client.post("/api/session/create", json={"backend": "redis"})
        assert resp.status_code == 400
        assert "redis" in resp.json()["detail"]

    def test_create_with_agent_override(self, client: TestClient, mock_runtime):
        """创建 session 时传入 agent override。"""
        resp = client.post(
            "/api/session/create",
            json={
                "workspace": "/ws",
                "agent": {
                    "model": "gpt-4o",
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
        assert override.model == "gpt-4o"
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
                    "model": "claude-sonnet-4-20250514",
                    "append_system_prompt": "\nAlways respond in Chinese.",
                },
            },
        )
        assert resp.status_code == 200
        call_kwargs = mock_runtime.create_session.call_args
        override = call_kwargs.kwargs["agent_override"]
        assert override.model == "claude-sonnet-4-20250514"
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
        mock_runtime.resume_session.assert_called_once_with("abc123")

    def test_resume_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.resume_session.side_effect = LookupError("Session not found: xxx")
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

        resp = client.get("/api/session/info", params={"session_id": "test-id"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["model"] == "gpt-4o"
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
        """Session 无 workspace 时 workdir 为 null。"""
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
        mock_session.agent.yolo = False
        mock_session.session_name = None
        mock_session.session_workspace = None
        mock_session.status = "idle"
        mock_session.agent.context_manager.get_context_stats.return_value = (0, 0)
        mock_session.agent.context_manager.get_skills_info.return_value = ""
        mock_session.agent.context_manager.system_prompt.content = ""
        mock_runtime.get_session.return_value = mock_session

        resp = client.get("/api/session/info", params={"session_id": "test-id"})
        assert resp.status_code == 200
        assert resp.json()["workdir"] is None

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
        mock_session.agent.yolo = False
        mock_session.session_name = None
        mock_session.session_workspace = None
        mock_session.status = "waiting"
        mock_session.agent.context_manager.get_context_stats.return_value = (0, 0)
        mock_session.agent.context_manager.get_skills_info.return_value = ""
        mock_session.agent.context_manager.system_prompt.content = ""
        mock_runtime.get_session.return_value = mock_session

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

    def test_update_model(self, client: TestClient, mock_runtime):
        """切换模型。"""
        mock_runtime.update_session = AsyncMock()
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "model": "gpt-4o-mini"},
        )
        assert resp.status_code == 200
        assert resp.json()["ok"] is True
        mock_runtime.update_session.assert_called_once_with(
            session_id="test-id",
            model="gpt-4o-mini",
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
                "model": "gpt-4o-mini",
                "title": "new title",
                "thinking": True,
                "yolo": True,
            },
        )
        assert resp.status_code == 200

    def test_update_all_none(self, client: TestClient, mock_runtime):
        """所有可选字段均为 None 返回 400。"""
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id"},
        )
        assert resp.status_code == 400

    def test_update_session_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.update_session = AsyncMock(
            side_effect=LookupError("session not found")
        )
        resp = client.post(
            "/api/session/update",
            json={"session_id": "xxx", "model": "gpt-4o"},
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
            model=None,
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
    """GET /api/models 测试。"""

    @patch("wing.gateway.routes.system.OpenAIProvider")
    def test_list_models_ok(self, mock_provider_cls, client: TestClient):
        """正常获取模型列表。"""
        mock_provider = MagicMock()

        async def mock_list_models():
            return ["gpt-4o", "gpt-4o-mini"]

        mock_provider.list_models = mock_list_models
        mock_provider_cls.return_value = mock_provider

        resp = client.get("/api/models")
        assert resp.status_code == 200
        data = resp.json()
        assert data["models"] == ["gpt-4o", "gpt-4o-mini"]

    @patch("wing.gateway.routes.system.OpenAIProvider")
    def test_list_models_error_returns_empty(
        self, mock_provider_cls, client: TestClient
    ):
        """调用失败时返回空列表。"""

        async def mock_list_models():
            raise RuntimeError("API error")

        mock_provider = MagicMock()
        mock_provider.list_models = mock_list_models
        mock_provider_cls.return_value = mock_provider

        resp = client.get("/api/models")
        assert resp.status_code == 200
        data = resp.json()
        assert data["models"] == []


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
        from wing.runtime import ReloadResult, ReloadResultItem

        mock_runtime.reload_system.return_value = ReloadResult(
            ok=True,
            items=[
                ReloadResultItem(name="config.yaml", ok=True),
                ReloadResultItem(name="hooks", ok=True),
                ReloadResultItem(name="prompt commands", ok=True),
                ReloadResultItem(name="provider", ok=True, detail="unchanged"),
                ReloadResultItem(name="skills & rules", ok=True),
            ],
        )

        resp = client.post("/api/system/reload")
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert len(data["results"]) == 5

    def test_reload_config_failure(self, client: TestClient, mock_runtime):
        """config 加载失败立即中止。"""
        from wing.runtime import ReloadResult, ReloadResultItem

        mock_runtime.reload_system.return_value = ReloadResult(
            ok=False,
            items=[ReloadResultItem(name="config.yaml", ok=False, detail="bad config")],
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
