# wing_gateway/server.py — Gateway 服务器

"""
Gateway 服务器——生命周期管理器 + EventBus 事件路由。

V2 实现（EventBus 模式）：
  - Gateway 不感知 session_id，只感知 client_id
  - Gateway 维护 {client_id: ws} 和 {ws: client_id} 两个 dict
  - Gateway subscribe EventBus，根据 EventTarget 转发给对应 ws
  - 断连时清理 Gateway 和 EventBus 的路由表

FastAPI app 创建和路由注册在 app.py 中完成（App Factory 模式）。
"""

from __future__ import annotations

import asyncio
import json
import socket
import sys
from datetime import datetime, timezone

from fastapi import WebSocket
import uvicorn

from wing.common.logger import log
from wing.config import AuthConfig, load_config
from wing.event import WingEvent, wire_dump
from wing.event_bus import event_bus
from wing.runtime import WingRuntime

from .app import create_app
from .remote_tools import RemoteToolManager

DEFAULT_PORT = 32523


def _check_port_available(host: str, port: int) -> bool:
    """检查端口是否可用，不可用时返回 False。

    使用 connect 而非 bind 来检测——避免 TIME_WAIT 误报。
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.settimeout(1)
            s.connect((host, port))
            # 能连上说明端口被占用
            return False
        except (OSError, socket.timeout):
            # 连不上说明端口空闲
            return True


class GatewayServer:
    """Gateway 服务器——生命周期管理器。

    持有 runtime、client 映射、FastAPI app。
    负责 start/stop 和 EventBus 事件路由。
    """

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = DEFAULT_PORT,
    ) -> None:
        self.host = host
        self.port = port
        self.runtime = WingRuntime()
        self._client_to_ws: dict[str, WebSocket] = {}  # client_id → ws
        self._ws_to_client: dict[WebSocket, str] = {}  # ws → client_id
        self._remote_tools = RemoteToolManager()  # 远程工具连接与调用中枢
        self._app = create_app(self)
        self._started_at = datetime.now(timezone.utc)

    @property
    def uptime(self) -> int:
        """Gateway 运行时长（秒）。"""
        return int((datetime.now(timezone.utc) - self._started_at).total_seconds())

    @property
    def auth_config(self) -> AuthConfig:
        """当前鉴权配置（每次读取最新单例，热重载后立即生效）。"""
        return load_config().gateway.auth

    def _warn_auth_lockout(self) -> None:
        """启动时检查 auth 配置，空 keys 锁死时发出警告。"""
        auth = self.auth_config
        if auth.enabled and not auth.keys:
            log.warning(
                "gateway.auth.enabled=true but keys list is empty — "
                "ALL requests (including /api/system/reload) will be "
                "rejected with 401. Edit config.yaml and restart to fix."
            )

    @property
    def clients(self) -> dict[str, WebSocket]:
        """client_id → WebSocket 映射，供 routes 访问。"""
        return self._client_to_ws

    @property
    def ws_to_clients(self) -> dict[WebSocket, str]:
        """WebSocket → client_id 映射，供 routes 访问。"""
        return self._ws_to_client

    @property
    def remote_tools(self) -> RemoteToolManager:
        """远程工具管理器，供 routes（ws / tools）访问。"""
        return self._remote_tools

    def start(self) -> None:
        """启动服务器（阻塞）。"""
        if not _check_port_available(self.host, self.port):
            print(f"❌ 端口 {self.port} 已被占用，请指定其他端口或释放占用。")
            sys.exit(1)

        self._warn_auth_lockout()

        # Subscribe EventBus
        event_bus.subscribe(self._on_event)

        print(f"🚀 Gateway 启动于 {self.host}:{self.port}")
        uvicorn.run(
            self._app,
            host=self.host,
            port=self.port,
            log_config=None,
        )

    def _on_event(self, event: WingEvent) -> None:
        """EventBus subscriber callback：根据 EventTarget 路由事件到 ws。

        同步回调，内部 asyncio.create_task 调度异步 send。
        tool_runtime（纯工具执行远端）不接收事件——global 广播与 client
        定向都跳过它，闭合"不参与事件订阅"的边界。

        帧内容在 create_task 之前 eager 序列化定型（统一 wire 出口
        `wire_dump`）：保证送达顺序等于发射顺序，且不依赖任何延迟序列化
        的时序（事件对象广播后不再被改写）。每个事件只序列化一次。
        """
        target = event.target
        if target is None:
            return

        payload = json.dumps(wire_dump(event))

        if target.scope == "global":
            # 发给所有 ws（跳过不收事件的 tool host）
            for cid, ws in list(self._client_to_ws.items()):
                if not self._receives_events(cid):
                    continue
                try:
                    asyncio.get_running_loop().create_task(self._send_text(ws, payload))
                except RuntimeError:
                    pass

        elif target.scope == "client":
            # 发给指定 client_ids 的 ws
            for cid in target.client_ids:
                ws = self._client_to_ws.get(cid)
                if ws is not None and self._receives_events(cid):
                    try:
                        asyncio.get_running_loop().create_task(
                            self._send_text(ws, payload)
                        )
                    except RuntimeError:
                        pass

    def _receives_events(self, client_id: str) -> bool:
        """client 是否接收事件。tool_runtime（attached 且 receives_events=False）
        被跳过；纯前端（未 attach）默认接收。"""
        return not (
            self._remote_tools.is_attached(client_id)
            and not self._remote_tools.receives_events(client_id)
        )

    async def _send_text(self, ws: WebSocket, data: str) -> None:
        """异步发送文本到 ws。"""
        try:
            await ws.send_text(data)
        except Exception as e:
            log.error(f"Failed to send to client: {e}")
