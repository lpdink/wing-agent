"""WS 错误帧的关联上下文。

被守的契约（`gateway/routes/ws.py`）：

- `ClientRequest` 帧处理失败时，错误帧携带**帧层关联上下文**——`session_id` 与
  客户端随帧发来的 `request_id`。前端据此把"服务端拒绝"关联回刚发出的那条消息
  （拒绝 → 丢弃 pending / 回填草稿），而不是把它当成一条无主的服务端错误。
- 帧层上下文**尚未设立**的失败（畸形帧：`ClientRequest` 解析失败）保持历史形状：
  无 `session_id`，`request_id` 是新生成的 uuid——不做猜测，也不把未校验字段
  当成关联来源。
"""

from __future__ import annotations

import json
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi import WebSocket
from starlette.websockets import WebSocketDisconnect

from _support import gateway_auth_config
from wing.common.utils import InvalidInputError
from wing.config import ApiKeyEntry

SESSION = "20250101-000000-abcdef01"
REQUEST = "req-correlated"

GHOST: dict[str, Any] = {
    "id": "b" * 64,
    "mime": "image/png",
    "bytes": 12,
    "width": 1,
    "height": 1,
    "name": "ghost.png",
}


def _make_server() -> Any:
    """造一个 GatewayServer（WingRuntime / load_config 都 mock 掉）。"""
    with (
        patch("wing.gateway.server.WingRuntime") as MockRuntime,
        patch("wing.gateway.server.load_config") as mock_load_config,
    ):
        MockRuntime.return_value = MagicMock()
        mock_load_config.return_value = gateway_auth_config(
            keys=[ApiKeyEntry(key="k", role="admin")]
        )
        from wing.gateway.server import GatewayServer

        return GatewayServer()


class _FrameWs:
    """脚本化 WS 客户端替身：按序吐出上行帧，记录所有出网帧。"""

    def __init__(self, frames: list[dict[str, Any]]) -> None:
        self._frames = [json.dumps(frame) for frame in frames]
        self.sent: list[dict[str, Any]] = []
        self.query_params: dict[str, str] = {}
        self.closed: list[tuple[int, str]] = []
        self.app = SimpleNamespace(state=SimpleNamespace(server=None))

    async def accept(self) -> None:
        pass

    async def send_json(self, data: dict[str, Any]) -> None:
        self.sent.append(data)

    async def send_text(self, data: str) -> None:
        self.sent.append(json.loads(data))

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self.closed.append((code, reason))

    async def receive_text(self) -> str:
        if not self._frames:
            # 脚本耗尽 = 客户端断开（handle_ws 的正常收尾路径）。
            raise WebSocketDisconnect(1000)
        return self._frames.pop(0)

    # ── 断言助手 ──

    def errors(self) -> list[dict[str, Any]]:
        return [frame for frame in self.sent if frame.get("type") == "error"]


async def _drive(server: Any, frames: list[dict[str, Any]]) -> _FrameWs:
    from wing.gateway.routes.ws import handle_ws

    ws = _FrameWs(frames)
    ws.app.state.server = server
    await handle_ws(cast("WebSocket", ws))
    return ws


class TestErrorFrameCorrelation:
    @pytest.mark.asyncio
    async def test_rejected_client_request_carries_frame_context(self):
        """ClientRequest 被拒：错误帧带客户端 request_id + 目标 session。"""
        server = _make_server()
        server.runtime.post = AsyncMock(
            side_effect=InvalidInputError(
                f"media reference not found in session {SESSION!r}: id={GHOST['id']!r}"
            )
        )

        ws = await _drive(
            server,
            [
                {
                    "request_id": REQUEST,
                    "session_id": SESSION,
                    "content": "ghost",
                    "media": [GHOST],
                }
            ],
        )

        errors = ws.errors()
        assert len(errors) == 1, ws.sent
        error = errors[0]
        assert error["request_id"] == REQUEST, error
        assert error["session_id"] == SESSION, error
        assert GHOST["id"][:8] in error["message"], error
        # 透传面：引用原样进 runtime.post（校验由下游闸门做，帧层不丢字段）。
        call = server.runtime.post.await_args
        assert call is not None, "runtime.post 未被调用"
        assert [ref.id for ref in call.kwargs["media"]] == [GHOST["id"]]

    @pytest.mark.asyncio
    async def test_malformed_frame_keeps_uncorrelated_shape(self):
        """解析失败的帧（上下文未设立）：无 session_id、request_id 为新 uuid。"""
        server = _make_server()

        ws = await _drive(server, [{"request_id": REQUEST, "oops": True}])

        errors = ws.errors()
        assert len(errors) == 1, ws.sent
        error = errors[0]
        assert "session_id" not in error, error
        assert error["request_id"] != REQUEST, error
        server.runtime.post.assert_not_called()

    @pytest.mark.asyncio
    async def test_server_error_after_frame_context_keeps_session(self):
        """非输入类故障（服务端异常）同样带上下文——归属与错误种类无关。"""
        server = _make_server()
        server.runtime.post = AsyncMock(side_effect=RuntimeError("boom"))

        ws = await _drive(
            server,
            [{"request_id": REQUEST, "session_id": SESSION, "content": "hi"}],
        )

        errors = ws.errors()
        assert len(errors) == 1, ws.sent
        assert errors[0]["request_id"] == REQUEST, errors[0]
        assert errors[0]["session_id"] == SESSION, errors[0]
        assert errors[0]["message"] == "boom", errors[0]
