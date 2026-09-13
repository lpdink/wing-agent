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

from wing.build_info import get_commit
from wing.common.logger import log
from wing.config import AuthConfig, load_config
from wing.event import WingEvent, wire_dump
from wing.event_bus import event_bus
from wing.runtime import WingRuntime

from .app import create_app
from .frames import HARD_LIMIT_BYTES, Frame, build_frames
from .remote_tools import RemoteToolManager

DEFAULT_PORT = 32523

# 单帧写超时（秒）：向一个客户端的单次发送超过它即判为慢/死消费者并回收。
# 硬编码——对"单帧 ≤8 MiB 的内网投递"极其宽裕（正常是毫秒级），对"永远不读"
# 的客户端足够快。不引入发送队列/背压池：投递模型仍是每事件一次 create_task。
WRITE_TIMEOUT_SECONDS = 60.0

# 回收时关闭连接的上界（秒）：对手正是"不读的客户端"，回收路径自己不能挂住。
CLOSE_TIMEOUT_SECONDS = 5.0


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

        print(
            f"🚀 Gateway 启动于 {self.host}:{self.port} (commit {get_commit() or 'unknown'})"
        )
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
        # 切分也在 eager 阶段完成（与序列化同理）：帧内容定型后再交给 task，
        # 不依赖任何延迟序列化的时序。小载荷原样单帧，零额外开销。
        frames = build_frames(payload, event.type)

        if target.scope == "global":
            # 发给所有 ws（跳过不收事件的 tool host）
            for cid, ws in list(self._client_to_ws.items()):
                if not self._receives_events(cid):
                    continue
                self._schedule_send(ws, frames, event.type)

        elif target.scope == "client":
            # 发给指定 client_ids 的 ws
            for cid in target.client_ids:
                ws = self._client_to_ws.get(cid)
                if ws is not None and self._receives_events(cid):
                    self._schedule_send(ws, frames, event.type)

    def _schedule_send(self, ws: WebSocket, frames: list[Frame], of_type: str) -> None:
        """调度一个事件的投递（每事件一次 task，同一事件的帧在该 task 内按序发出）。"""
        try:
            asyncio.get_running_loop().create_task(
                self._send_frames(ws, frames, of_type)
            )
        except RuntimeError:
            pass  # 无运行中的事件循环（进程收尾）——与既有行为一致

    def _receives_events(self, client_id: str) -> bool:
        """client 是否接收事件。tool_runtime（attached 且 receives_events=False）
        被跳过；纯前端（未 attach）默认接收。"""
        return not (
            self._remote_tools.is_attached(client_id)
            and not self._remote_tools.receives_events(client_id)
        )

    async def drop_client(self, ws: WebSocket, *, reason: str) -> str | None:
        """回收一个客户端连接——**唯一的清理入口**，幂等。

        两条路径共用：`handle_ws` 的正常断连收尾（客户端已经走了，不需要
        再关连接）与慢消费者回收（写超时/写失败，调用方随后主动关连接）。
        两份平行实现必然漂移——`fail_client` 的 KV cache 保护、路由表清理
        这些不变量只能有一个家。

        幂等靠 ``ws_to_clients.pop`` 的返回值：只有"第一次拿到回收权"的
        调用者产生副作用（关连接、fail_client、日志）。并发的多个
        `_send_text` 同时失败、或发送失败紧接 `handle_ws` 的 finally 时，
        只有一个赢家——这也是"每个死客户端最多一条回收日志"的结构性保证。

        返回 client_id（首次回收）；已被回收 / 从未登记返回 None。
        """
        client_id = self._ws_to_client.pop(ws, None)
        if client_id is None:
            return None
        self._client_to_ws.pop(client_id, None)
        event_bus.route_detach_client(client_id)
        if self._remote_tools.is_attached(client_id):
            # 在途调用立即失败 + 注销远程工具（敏锐检测断连）
            self._remote_tools.fail_client(client_id, reason)
        log.info(f"Client disconnected: {client_id} ({reason})")
        return client_id

    async def _recycle_client(self, ws: WebSocket, reason: str) -> None:
        """回收慢/死消费者：先注销（投递立即停止），再尽力关闭连接。"""
        client_id = await self.drop_client(ws, reason=reason)
        if client_id is None:
            return  # 已被回收——不重复关闭
        try:
            await asyncio.wait_for(
                ws.close(code=1013, reason="slow consumer"),
                timeout=CLOSE_TIMEOUT_SECONDS,
            )
        except Exception as e:
            # 关闭失败无需补救：路由与投递列表已经清干净，连接由 ASGI 层收尸。
            # 回收路径绝不能因为对端不读而挂住。
            log.debug(f"Failed to close recycled client {client_id}: {e}")

    async def _send_frames(
        self, ws: WebSocket, frames: list[Frame], of_type: str
    ) -> None:
        """一个事件的投递单元：同一事件的帧在**同一个 task 内**按序发出。

        硬上限（16 MiB，= 客户端单帧上限）是最终契约：任何仍超限的帧在这里
        被拦截丢弃 + 一行 WARN（应用层照常 emit，丢的只是这一帧；不新增计数
        指标）。切分正常时每帧 ≤ 软上限，本分支是安全网而非控制流。

        客户端被回收后（写超时 / 写失败）立即停止剩余帧——不产生二次回收、
        不产生重复失败日志。
        """
        client_id = self._ws_to_client.get(ws, "unknown")
        for frame in frames:
            if frame.size > HARD_LIMIT_BYTES:
                log.warning(
                    f"Dropping oversized frame for client {client_id}: "
                    f"type={of_type} size={frame.size} > {HARD_LIMIT_BYTES}"
                )
                continue
            if not await self._send_text(ws, frame.text):
                return

    async def _send_text(self, ws: WebSocket, data: str) -> bool:
        """异步发送文本到 ws——有界等待，超时/失败即回收该客户端。

        投递模型未变（每事件一次 create_task，帧逐次发送）。这里的上界是
        "无限期"与"有限期"的分界：写超时（或写失败）后该 client 从路由表
        消失，后续事件不再投递（`_on_event` 的投递列表就是路由表），因此不会
        再有失败投递与日志洪泛。

        返回该客户端是否**仍然有效**：False 表示已被回收，调用方（分片发送
        循环）不应继续尝试投递剩余帧。

        边界说明：uvicorn 的 WS 实现把未写完的数据放进用户态缓冲，
        `send_text` 往往立即返回——此时真正触发回收的是异常分支（连接已死 /
        ASGI 已关闭）。两条分支走同一条回收路径，行为一致。
        """
        try:
            await asyncio.wait_for(ws.send_text(data), timeout=WRITE_TIMEOUT_SECONDS)
            return True
        except TimeoutError:
            log.error(
                f"Dropping slow client: send blocked for >{WRITE_TIMEOUT_SECONDS:.0f}s"
            )
            await self._recycle_client(
                ws, f"write timeout after {WRITE_TIMEOUT_SECONDS:.0f}s"
            )
            return False
        except Exception as e:
            log.error(f"Failed to send to client: {e}")
            await self._recycle_client(ws, f"send failed: {e}")
            return False
