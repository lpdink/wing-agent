"""Gateway HTTP 端点单元测试。

使用 FastAPI TestClient + Mock WingRuntime 测试所有 HTTP 路由。
不启动真实 uvicorn server，不依赖端口 32523。
"""

from __future__ import annotations

from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, patch

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
            template_name=None, workspace=None, agent_override=None
        )

    def test_create_with_template(self, client: TestClient, mock_runtime):
        """指定模板创建 session。"""
        resp = client.post(
            "/api/session/create",
            json={"template_name": "coder", "workspace": "/ws"},
        )
        assert resp.status_code == 200
        mock_runtime.create_session.assert_called_once_with(
            template_name="coder", workspace="/ws", agent_override=None
        )

    def test_create_template_not_found(self, client: TestClient, mock_runtime):
        """模板不存在返回 400。"""
        mock_runtime.create_session.side_effect = ValueError("template 'xxx' not found")
        resp = client.post("/api/session/create", json={"template_name": "xxx"})
        assert resp.status_code == 400

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
        mock_session.agent.context_manager.get_context_stats.return_value = (5, 800)
        mock_session.agent.context_manager.get_skills_info.return_value = (
            "skill-a: desc"
        )
        mock_runtime.sm.get_session.return_value = mock_session

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
        assert data["context_stats"]["message_count"] == 5
        assert data["context_stats"]["total_tokens"] == 800
        assert data["skills_info"] == "skill-a: desc"

    def test_info_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.sm.get_session.return_value = None
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
        mock_runtime.sm.get_session.return_value = mock_session

        resp = client.get("/api/session/branches", params={"session_id": "test-id"})
        assert resp.status_code == 200
        data = resp.json()
        assert len(data["targets"]) == 2
        assert data["targets"][0]["uuid"] == "msg-1"
        assert data["targets"][0]["content"] == "hello"

    def test_branches_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.sm.get_session.return_value = None
        resp = client.get("/api/session/branches", params={"session_id": "xxx"})
        assert resp.status_code == 404


# ============================================================
# Session Update
# ============================================================


class TestSessionUpdate:
    """POST /api/session/update 测试。"""

    def _make_mock_session(self, mock_runtime):
        """创建 mock session 和 template_manager。"""
        mock_session = MagicMock()
        mock_session.session_id = "test-id"
        mock_session.session_name = "test-session"
        mock_session.template_name = "default"
        mock_session.agent.model = "gpt-4o"
        mock_session.agent.yolo = False
        mock_session.agent.model_provider.thinking = False
        # set_thinking / set_yolo 需实际更新属性，否则 emit 读到旧值
        mock_session.agent.model_provider.set_thinking.side_effect = lambda v: setattr(
            mock_session.agent.model_provider, "thinking", v
        )
        mock_session.agent.model_provider.reasoning_effort = None
        mock_session.agent.set_reasoning_effort.side_effect = lambda v: setattr(
            mock_session.agent.model_provider, "reasoning_effort", v
        )
        mock_session.agent.set_yolo.side_effect = lambda v: setattr(
            mock_session.agent, "yolo", v
        )
        mock_session.switch_template = AsyncMock()
        mock_runtime.sm.get_session.return_value = mock_session

        mock_template = MagicMock()
        mock_runtime.template_manager.get.return_value = mock_template
        mock_runtime.template_manager.all_names = ["default", "coder"]
        return mock_session

    def test_update_model(self, client: TestClient, mock_runtime):
        """切换模型。"""
        mock_session = self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "model": "gpt-4o-mini"},
        )
        assert resp.status_code == 200
        assert resp.json()["ok"] is True
        assert mock_session.agent.model == "gpt-4o-mini"

    def test_update_agent(self, client: TestClient, mock_runtime):
        """切换 agent 模板。"""
        mock_session = self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "agent": "coder"},
        )
        assert resp.status_code == 200
        mock_session.switch_template.assert_called_once()

    def test_update_agent_not_found(self, client: TestClient, mock_runtime):
        """模板不存在返回 400。"""
        self._make_mock_session(mock_runtime)
        mock_runtime.template_manager.get.return_value = None
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "agent": "nonexistent"},
        )
        assert resp.status_code == 400
        assert "nonexistent" in resp.json()["detail"]

    def test_update_title(self, client: TestClient, mock_runtime):
        """设置 session 名称。"""
        mock_session = self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "title": "my session"},
        )
        assert resp.status_code == 200
        mock_session.set_title.assert_called_once_with("my session")

    def test_update_thinking(self, client: TestClient, mock_runtime):
        """开关 thinking 模式。"""
        mock_session = self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "thinking": True},
        )
        assert resp.status_code == 200
        mock_session.agent.model_provider.set_thinking.assert_called_once_with(True)

    def test_update_reasoning_effort(self, client: TestClient, mock_runtime):
        """设置推理力度。"""
        mock_session = self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "reasoning_effort": "high"},
        )
        assert resp.status_code == 200
        mock_session.agent.set_reasoning_effort.assert_called_once_with("high")

    def test_update_yolo(self, client: TestClient, mock_runtime):
        """开关 yolo 模式。"""
        mock_session = self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "yolo": True},
        )
        assert resp.status_code == 200
        mock_session.agent.set_yolo.assert_called_once_with(True)

    def test_update_multi_fields(self, client: TestClient, mock_runtime):
        """多字段同时更新，按正确顺序执行。"""
        mock_session = self._make_mock_session(mock_runtime)
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
        # agent 在前，model 在后
        mock_session.switch_template.assert_called_once()
        assert mock_session.agent.model == "gpt-4o-mini"
        mock_session.set_title.assert_called_once_with("new title")
        mock_session.agent.model_provider.set_thinking.assert_called_once_with(True)
        mock_session.agent.set_yolo.assert_called_once_with(True)

    def test_update_all_none(self, client: TestClient, mock_runtime):
        """所有可选字段均为 None 返回 400。"""
        self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id"},
        )
        assert resp.status_code == 400

    def test_update_session_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.sm.get_session.return_value = None
        resp = client.post(
            "/api/session/update",
            json={"session_id": "xxx", "model": "gpt-4o"},
        )
        assert resp.status_code == 404

    @patch("wing.gateway.routes.session.event_bus")
    def test_update_model_emits_state_changed_event(
        self, mock_bus, client: TestClient, mock_runtime
    ):
        """更新模型后 emit SessionStateChangedEvent，仅携带变更字段。"""
        mock_session = self._make_mock_session(mock_runtime)
        mock_session.agent.model = "gpt-4o-mini"  # 更新后的值
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "model": "gpt-4o-mini"},
        )
        assert resp.status_code == 200
        mock_bus.emit.assert_called_once()
        event = mock_bus.emit.call_args[0][0]
        assert event.type == "session_state_changed"
        assert event.model == "gpt-4o-mini"
        assert event.thinking is None
        assert event.yolo is None
        assert event.title is None
        assert event.agent is None

    @patch("wing.gateway.routes.session.event_bus")
    def test_update_multi_fields_emits_state_changed_event(
        self, mock_bus, client: TestClient, mock_runtime
    ):
        """多字段同时更新后 emit SessionStateChangedEvent，携带所有变更字段。"""
        mock_session = self._make_mock_session(mock_runtime)
        mock_session.agent.model = "gpt-4o-mini"
        mock_session.session_name = "new title"
        mock_session.template_name = "coder"
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
        mock_bus.emit.assert_called_once()
        event = mock_bus.emit.call_args[0][0]
        assert event.type == "session_state_changed"
        assert event.model == "gpt-4o-mini"
        assert event.thinking is True
        assert event.yolo is True
        assert event.title == "new title"
        assert event.agent == "coder"

    @patch("wing.gateway.routes.session.event_bus")
    def test_update_thinking_emits_state_changed_event(
        self, mock_bus, client: TestClient, mock_runtime
    ):
        """切换 thinking 后 emit SessionStateChangedEvent(thinking=True)。"""
        self._make_mock_session(mock_runtime)
        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "thinking": True},
        )
        assert resp.status_code == 200
        mock_bus.emit.assert_called_once()
        event = mock_bus.emit.call_args[0][0]
        assert event.type == "session_state_changed"
        assert event.model is None
        assert event.thinking is True

    @patch("wing.gateway.routes.session.event_bus")
    def test_update_agent_emits_reset_state_fields(
        self, mock_bus, client: TestClient, mock_runtime
    ):
        """Agent 切换后 emit 事件携带 switch_template 重置后的 thinking/yolo/reasoning_effort。"""
        mock_session = self._make_mock_session(mock_runtime)
        # switch_template 后 agent 的状态（模拟新 template 的默认值）
        mock_session.agent.model_provider.thinking = True
        mock_session.agent.model_provider.reasoning_effort = "medium"
        mock_session.agent.yolo = False
        mock_session.agent.model = "gpt-4o"
        mock_session.template_name = "coder"

        resp = client.post(
            "/api/session/update",
            json={"session_id": "test-id", "agent": "coder"},
        )
        assert resp.status_code == 200
        mock_bus.emit.assert_called_once()
        event = mock_bus.emit.call_args[0][0]
        assert event.type == "session_state_changed"
        assert event.agent == "coder"
        assert event.model == "gpt-4o"
        # agent switch 应报告重置后的值，而非 None
        assert event.thinking is True
        assert event.reasoning_effort == "medium"
        assert event.yolo is False
        assert event.title is None


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
        mock_session = MagicMock()
        mock_session.session_id = "test-id"
        mock_session.agent.model = "gpt-4o"
        mock_compactor = MagicMock()
        mock_compacted = MagicMock()
        mock_compacted.content = "summary"
        mock_compacted.usage.prompt_tokens = 1000
        mock_compacted.usage.completion_tokens = 200
        mock_compactor.do_compact = AsyncMock(return_value=mock_compacted)
        mock_session.agent.context_manager.compactor = mock_compactor
        mock_session.agent.context_manager._messages = MagicMock()
        mock_session.agent.context_manager._messages.__iter__ = MagicMock(
            return_value=iter([])
        )
        mock_session.agent.context_manager.system_prompt = MagicMock()
        mock_session.agent.context_manager.get_context_stats.return_value = (2, 200)
        mock_runtime.sm.get_session.return_value = mock_session

        resp = client.post("/api/session/compact", json={"session_id": "test-id"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert data["original_tokens"] == 1000
        assert data["compressed_tokens"] == 200

    def test_compact_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.sm.get_session.return_value = None
        resp = client.post("/api/session/compact", json={"session_id": "xxx"})
        assert resp.status_code == 404

    def test_compact_no_compactor(self, client: TestClient, mock_runtime):
        """未配置 compactor 返回 400。"""
        mock_session = MagicMock()
        mock_session.agent.context_manager.compactor = None
        mock_runtime.sm.get_session.return_value = mock_session

        resp = client.post("/api/session/compact", json={"session_id": "test-id"})
        assert resp.status_code == 400


# ============================================================
# Session: Interrupt
# ============================================================


class TestSessionInterrupt:
    """POST /api/session/interrupt 测试。"""

    def test_interrupt_ok(self, client: TestClient, mock_runtime):
        """正常中断。"""
        mock_session = MagicMock()
        mock_session.session_id = "test-id"
        mock_runtime.sm.get_session.return_value = mock_session

        resp = client.post("/api/session/interrupt", json={"session_id": "test-id"})
        assert resp.status_code == 200
        assert resp.json()["ok"] is True
        mock_session.agent.interrupt.assert_called_once()

    def test_interrupt_not_found(self, client: TestClient, mock_runtime):
        """Session 不存在返回 404。"""
        mock_runtime.sm.get_session.return_value = None
        resp = client.post("/api/session/interrupt", json={"session_id": "xxx"})
        assert resp.status_code == 404


# ============================================================
# Session: Rewind
# ============================================================


class TestSessionRewind:
    """POST /api/session/rewind 测试。"""

    def test_rewind_ok(self, client: TestClient, mock_runtime):
        """正常回退。"""
        mock_session = MagicMock()
        mock_session.session_id = "test-id"
        mock_session.agent.context_manager.rewind.return_value = "draft text"
        mock_session.agent.context_manager.get_context_stats.return_value = (3, 500)
        mock_session.agent.context_manager.get_context_window.return_value = []
        mock_session.agent.context_manager.get_branch_targets.return_value = []
        mock_session.agent.context_manager.compactor = None
        mock_runtime.sm.get_session.return_value = mock_session

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
        mock_runtime.sm.get_session.return_value = None
        resp = client.post(
            "/api/session/rewind",
            json={"session_id": "xxx", "target_uuid": "abc"},
        )
        assert resp.status_code == 404

    def test_rewind_invalid_uuid(self, client: TestClient, mock_runtime):
        """无效 uuid 返回 400。"""
        mock_session = MagicMock()
        mock_session.agent.context_manager.rewind.side_effect = ValueError(
            "uuid not found"
        )
        mock_runtime.sm.get_session.return_value = mock_session

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

    @patch("wing.magic_command.prompt_commands.register_prompt_commands")
    @patch("wing.config.load_hooks")
    @patch("wing.config.load_config")
    def test_reload_ok(
        self, mock_load_config, _mock_load_hooks, _mock_register, client, mock_runtime
    ):
        """全部重载成功。"""
        mock_config = MagicMock()
        mock_config.hooks = []
        mock_config.commands.paths = []
        mock_load_config.return_value = mock_config
        mock_runtime.sm._sessions = {}

        resp = client.post("/api/system/reload")
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert len(data["results"]) == 5

    @patch("wing.config.load_config")
    def test_reload_config_failure(self, mock_load_config, client, mock_runtime):
        """config 加载失败立即中止。"""
        mock_load_config.side_effect = Exception("bad config")

        resp = client.post("/api/system/reload")
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is False
        assert len(data["results"]) == 1
        assert data["results"][0]["name"] == "config.yaml"
        assert data["results"][0]["ok"] is False
