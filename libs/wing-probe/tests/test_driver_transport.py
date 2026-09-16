"""driver 传输层装配自测：``GatewayWS._read_loop → _dispatch`` 与 HTTP 留档缝。

两条都是"声明了但没人守"的装配前提，用**假对象**在进程内压住（不起网关）：

1. ``GatewayWS`` 的读任务（tasks 4.2）：假连接按脚本吐帧，断言
   - ``_chunk`` 信封合并后的 ``Delivery.text`` == 原始载荷、``Event.frames > 1``
     （spec「大事件帧可重组」在本层的可测面；Rust 侧同样是 16 MiB 软上限切分）；
   - 畸形帧走 ``invalid_frame`` 合成类型（不静默丢帧），``close_reason`` 可读；
2. ``DriverHttp`` 的留档缝（tasks 4.1，review N4）：桩网关 + **公开** ``GatewayClient``
   方法，断言"首个调用必须被留档"——上游若不再调用 ``_post`` / ``_get``，
   留档会静默全空，这条测试先红。
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator, Sequence
from pathlib import Path

import pytest
import pytest_asyncio
from aiohttp import web

from wing_probe.driver import (
    CHUNK_TYPE,
    DEFAULT_LIMITS,
    Delivery,
    Driver,
    DriverHttp,
    DriverHttpError,
    GatewayWS,
)
from wing_probe.watch.timeline import FrameLog
from websockets.exceptions import ConnectionClosed
from websockets.frames import Close


class FakeConnection:
    """假 WS 连接：按脚本吐帧，脚本耗尽后抛 ``ConnectionClosed``（读任务据此收尾）。"""

    def __init__(self, frames: Sequence[str], *, close_code: int = 1000) -> None:
        self._frames = list(frames)
        self._close_code = close_code
        self.closed = False
        self.sent: list[str] = []

    async def recv(self) -> str:
        if not self._frames:
            raise ConnectionClosed(Close(self._close_code, "scripted end"), None)
        return self._frames.pop(0)

    async def send(self, text: str) -> None:
        self.sent.append(text)

    async def close(self) -> None:
        self.closed = True


class FakeEnv:
    """``Driver`` 需要的 ``ProbeEnv`` 最小面（``EnvLike`` 协议）。"""

    def __init__(self, root: Path, gateway_url: str = "http://127.0.0.1:1") -> None:
        self.root = root
        self.gateway_url = gateway_url
        self.started_at = 0.0

    @property
    def artifacts_path(self) -> Path:
        return self.root / "artifacts"

    def session_dir(self, session_id: str) -> Path:
        return self.root / "sessions" / session_id


def chunk_frame(payload: str, *, index: int, count: int, chunk_id: str = "1") -> str:
    """网关侧信封帧的线上形状（``type`` 在帧头，与 ``gateway/frames.py`` 一致）。"""
    return (
        '{"type":"_chunk"'
        f',"id":{json.dumps(chunk_id)}'
        f',"index":{index}'
        f',"count":{count}'
        ',"of_type":"sync_session"'
        f',"data":{json.dumps(payload)}}}'
    )


async def run_read_loop(driver: Driver, frames: Sequence[str]) -> GatewayWS:
    """用假连接跑完整读任务并等它退出（返回已收尾的客户端）。"""
    client = GatewayWS(
        FakeConnection(frames),  # ty: ignore[invalid-argument-type]
        client_id="fake-client",
        handler=driver._on_event,
        frames=FrameLog(),
        limits=DEFAULT_LIMITS,
        started_at=driver.env.started_at,
    )
    client._read_task = asyncio.create_task(client._read_loop())
    assert await client.wait_closed(2.0), "读任务未在时限内退出"
    return client


@pytest.mark.asyncio
async def test_chunked_frames_are_merged_and_dispatched(tmp_path: Path) -> None:
    """分片合并：Delivery 文本 == 原始载荷、事件 frames > 1、帧日志标注信封帧。"""
    driver = Driver(FakeEnv(tmp_path))
    session = await driver.attach("sid-1", subscribe=False)
    payload = json.dumps(
        {
            "type": "sync_session",
            "session_id": "sid-1",
            "messages": [{"role": "user", "content": "x" * 200}],
        },
        ensure_ascii=False,
    )
    parts = [payload[:60], payload[60:140], payload[140:]]

    client = await run_read_loop(
        driver,
        [
            chunk_frame(parts[0], index=0, count=3),
            chunk_frame(parts[1], index=1, count=3),
            chunk_frame(parts[2], index=2, count=3),
        ],
    )

    events = session.timeline.all()
    assert [event.type for event in events] == ["sync_session"], events
    merged = events[0]
    assert merged.raw == payload, "合并后的载荷必须逐字节等于原始事件文本"
    assert merged.frames == 3, "分片重组必须留痕（frames > 1）"
    assert merged.data["messages"][0]["content"] == "x" * 200
    assert client.frames_received == 3
    assert client.events_received == 1
    assert [frame.chunk for frame in client.frames] == [True, True, True]
    assert client.assembling is False
    assert "connection closed by gateway" in (client.close_reason or "")

    # 后续普通帧直通（窗口已闭合）
    assert client._reassembler.on_text('{"type":"text"}', at=1.0) == [
        Delivery('{"type":"text"}', 1.0)
    ]
    await driver.close()


@pytest.mark.asyncio
async def test_malformed_frames_take_invalid_frame_path(tmp_path: Path) -> None:
    """畸形帧：合成 ``invalid_frame`` 事件（带原因与原文），不静默丢帧。"""
    driver = Driver(FakeEnv(tmp_path))
    session = await driver.attach("sid-1", subscribe=False)

    client = await run_read_loop(
        driver,
        [
            "not json at all",
            '{"no_type": 1}',
            '["array", "is", "not", "an", "object"]',
            '{"type":"text","session_id":"sid-1","content":"ok"}',
        ],
    )

    # 没有 session_id 的畸形帧进 driver 级时间线；正常帧仍投给会话。
    assert [event.type for event in driver.timeline.all()] == [
        "invalid_frame",
        "invalid_frame",
        "invalid_frame",
    ], driver.timeline.types()
    reasons = [event.data["error"] for event in driver.timeline.all()]
    assert "not valid JSON" in reasons[0], reasons
    assert "'type' field" in reasons[1], reasons
    assert "not a JSON object" in reasons[2], reasons
    assert driver.timeline.all()[0].data["raw"] == "not json at all"

    assert [event.type for event in session.timeline.all()] == ["text"]
    assert session.timeline.all()[0].data["content"] == "ok"
    assert client.close_reason is not None and "gateway" in client.close_reason
    await driver.close()


# ── HTTP 留档缝（review N4） ────────────────────────────────


class StubGateway:
    """桩网关：只服务 driver 装配测试需要的两个端点。"""

    def __init__(self) -> None:
        self.resume_workspace = "/tmp/probe-stub-workspace"
        self.runner: web.AppRunner | None = None
        self.port = 0

    async def start(self) -> StubGateway:
        app = web.Application()
        app.router.add_get("/api/health", self._health)
        app.router.add_post("/api/session/resume", self._resume)
        app.router.add_post("/api/session/subscribe", self._ok)
        app.router.add_post("/api/session/boom", self._boom)
        self.runner = web.AppRunner(app, access_log=None)
        await self.runner.setup()
        site = web.TCPSite(self.runner, "127.0.0.1", 0)
        await site.start()
        self.port = _bound_port(site)
        return self

    async def stop(self) -> None:
        if self.runner is not None:
            await self.runner.cleanup()
            self.runner = None

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    async def _health(self, request: web.Request) -> web.Response:
        return web.json_response({"status": "ok"})

    async def _resume(self, request: web.Request) -> web.Response:
        body = await request.json()
        return web.json_response(
            {
                "session_id": body["session_id"],
                "template_name": "default",
                "workspace": self.resume_workspace,
            }
        )

    async def _ok(self, request: web.Request) -> web.Response:
        return web.json_response({"ok": True})

    async def _boom(self, request: web.Request) -> web.Response:
        return web.json_response({"detail": "stub exploded"}, status=500)


def _bound_port(site: web.TCPSite) -> int:
    """读回 ``TCPSite`` 实际绑定端口（``port=0`` → OS 分配）。"""
    server = getattr(site, "_server", None)
    sockets = getattr(server, "sockets", None)
    assert sockets, "TCPSite 尚未绑定端口"
    return int(sockets[0].getsockname()[1])


@pytest_asyncio.fixture
async def stub_gateway() -> AsyncIterator[StubGateway]:
    stub = await StubGateway().start()
    try:
        yield stub
    finally:
        await stub.stop()


@pytest.mark.asyncio
async def test_first_public_call_is_logged(stub_gateway: StubGateway) -> None:
    """公开方法（``health``）走的是被覆盖的出网口——首个调用必须留下留档。"""
    http = DriverHttp(stub_gateway.url, started_at=0.0)

    assert http.calls == [], "构造不留档"
    response = await http.health()

    assert response == {"status": "ok"}
    assert len(http.calls) == 1, (
        "首个公开调用没有被留档：wing_sdk.GatewayClient 不再走 _post/_get 缝了？"
    )
    call = http.calls[0]
    assert (call.method, call.path, call.status) == ("GET", "/api/health", 200)
    assert call.response == {"status": "ok"}
    assert call.ok is True
    assert http.last_call(path="/api/health") == call
    await http.close()


@pytest.mark.asyncio
async def test_failed_call_is_logged_and_reported(stub_gateway: StubGateway) -> None:
    """非 2xx：抛错但**仍留档**（含状态码 / 响应体 / 原文）。"""
    http = DriverHttp(stub_gateway.url, started_at=0.0)

    with pytest.raises(DriverHttpError) as failure:
        await http._call("POST", "/api/session/boom", body={"session_id": "s"})

    assert failure.value.status == 500
    assert "stub exploded" in str(failure.value)
    assert len(http.calls) == 1
    assert http.calls[0].status == 500
    assert http.calls[0].response == {"detail": "stub exploded"}
    await http.close()


@pytest.mark.asyncio
async def test_resume_backfills_workspace_and_response(
    tmp_path: Path, stub_gateway: StubGateway
) -> None:
    """``Driver.resume``：workspace 从响应回填，重复挂载刷新 response（review N2/N5）。"""
    env = FakeEnv(tmp_path, gateway_url=stub_gateway.url)
    driver = Driver(env)
    try:
        session = await driver.resume("sid-1", subscribe=False)

        # Session.workspace 是 resolve() 后的绝对路径（macOS 上 /tmp → /private/tmp）
        assert session.workspace == Path(stub_gateway.resume_workspace).resolve()
        assert session.response["template_name"] == "default"

        # 再挂载一次：句柄复用，但 response 刷新为新响应（不残留旧值）。
        stub_gateway.resume_workspace = "/tmp/other"
        again = await driver.resume("sid-1", subscribe=False)
        assert again is session
        assert again.response["workspace"] == "/tmp/other"
        assert again.workspace == Path("/tmp/probe-stub-workspace").resolve(), (
            "已确认的 workspace 不被静默改写"
        )
    finally:
        await driver.close()


@pytest.mark.asyncio
async def test_attach_refreshes_response_without_losing_events(tmp_path: Path) -> None:
    """``attach`` 已挂载路径：response/workspace 刷新，时间线与游标不受影响。"""
    env = FakeEnv(tmp_path)
    driver = Driver(env)
    try:
        session = await driver.attach(
            "sid-1", response={"draft": "old"}, subscribe=False
        )
        session.timeline.append("text", {"session_id": "sid-1", "content": "hi"})

        same = await driver.attach(
            "sid-1",
            response={"draft": "fresh"},
            workspace=tmp_path / "ws",
            subscribe=False,
        )

        assert same is session
        assert session.response == {"draft": "fresh"}
        assert session.workspace == (tmp_path / "ws").resolve()
        assert [event.type for event in session.timeline.all()] == ["text"]
    finally:
        await driver.close()


def test_chunk_type_constant_matches_gateway() -> None:
    """信封 type 常量的线上值（防止与网关/文档漂移）。"""
    assert CHUNK_TYPE == "_chunk"
