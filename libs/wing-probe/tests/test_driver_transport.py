"""driver 传输层装配自测：``GatewayWS._read_loop → _dispatch`` 与 HTTP 留档缝。

两条都是"声明了但没人守"的装配前提，用**假对象**在进程内压住（不起网关）：

1. ``GatewayWS`` 的读任务（tasks 4.2）：假连接按脚本吐帧，断言
   - ``_chunk`` 信封合并后的 ``Delivery.text`` == 原始载荷、``Event.frames > 1``
     （spec「大事件帧可重组」在本层的可测面；Rust 侧同样是 16 MiB 软上限切分）；
   - 畸形帧走 ``invalid_frame`` 合成类型（不静默丢帧），``close_reason`` 可读；
2. ``DriverHttp`` 的留档缝（tasks 4.1）：桩网关 + **公开** ``GatewayClient``
   方法，断言"首个调用必须被留档"——上游若不再调用 ``_post`` / ``_get``，
   留档会静默全空，这条测试先红。
"""

from __future__ import annotations

import asyncio
import hashlib
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


class ScriptedConnection:
    """假连接：按 ``(text, delay)`` 脚本吐帧（delay 模拟分片间隔），耗尽后抛 ``ConnectionClosed``。"""

    def __init__(self, script: Sequence[tuple[str, float]]) -> None:
        self._script = list(script)

    async def recv(self) -> str:
        if not self._script:
            raise ConnectionClosed(Close(1000, "scripted end"), None)
        text, delay = self._script.pop(0)
        if delay:
            await asyncio.sleep(delay)
        return text

    async def send(self, text: str) -> None:  # pragma: no cover - 未使用
        pass

    async def close(self) -> None:  # pragma: no cover - 未使用
        pass


class FrozenClock:
    """冻结时钟：``started_at`` 与 ``clock()`` 取不同值，暴露尺度混用。"""

    def __init__(self, value: float) -> None:
        self.value = value

    def __call__(self) -> float:
        return self.value


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
async def test_chunk_deadline_uses_the_same_scale_as_on_text(tmp_path: Path) -> None:
    """分片不闭合超时用 ``now()``（相对 started_at）查询——尺度混用即误报 stalled。

    回归素材：``_read_loop`` 把 ``at=self.now()`` 喂给 ``Reassembler``，deadline
    因此是**相对**尺度；``_recv_frame`` 若用原始 ``_clock()`` 查询，started_at 一旦
    非零（真实 ProbeEnv 就是这样）"还有 30s" 会被算成"已过期"，第一个分片到达后
    立刻断开——大帧整机场景首次走通 `_chunk` 重组时抓到。
    """
    driver = Driver(FakeEnv(tmp_path))
    session = await driver.attach("sid-1", subscribe=False)
    payload = json.dumps(
        {"type": "sync_session", "session_id": "sid-1", "messages": []},
        ensure_ascii=False,
    )
    parts = [payload[:40], payload[40:]]

    client = GatewayWS(
        ScriptedConnection(  # ty: ignore[invalid-argument-type]
            [
                (chunk_frame(parts[0], index=0, count=2), 0.0),
                (chunk_frame(parts[1], index=1, count=2), 0.05),
                ('{"type":"text","session_id":"sid-1","content":"after"}', 0.0),
            ]
        ),
        client_id="fake-client",
        handler=driver._on_event,
        frames=FrameLog(),
        started_at=500.0,  # 与 clock() 错开：now() 才是 reassembler 的尺度
        clock=FrozenClock(1000.0),
    )
    client._read_task = asyncio.create_task(client._read_loop())
    assert await client.wait_closed(2.0), "读任务未在时限内退出"

    assert "reassembly failed" not in (client.close_reason or ""), (
        "把还有 30s 的分片窗口误判为过期（尺度混用）",
        client.close_reason,
    )
    assert "connection closed by gateway" in (client.close_reason or "")
    events = session.timeline.all()
    assert [event.type for event in events] == ["sync_session", "text"], events
    assert events[0].frames == 2, events[0].frames
    assert events[0].raw == payload
    assert events[1].data["content"] == "after"
    assert client.assembling is False
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


# ── HTTP 留档缝 ────────────────────────────────


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
        app.router.add_get("/api/tools", self._tools)
        app.router.add_post("/api/session/update", self._echo)
        app.router.add_post("/api/session/release", self._echo)
        app.router.add_post("/api/session/media", self._media)
        app.router.add_post("/api/session/boom", self._boom)
        app.router.add_post("/api/session/text", self._text)
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

    async def _echo(self, request: web.Request) -> web.Response:
        """回显请求体（driver 具名动作的请求形状断言用）。"""
        return web.json_response({"ok": True, "echo": await request.json()})

    async def _media(self, request: web.Request) -> web.Response:
        """回显**原始字节体**的摘要 + query（``upload_media`` 的请求形状断言用）。"""
        raw = await request.read()
        return web.json_response(
            {
                "ok": True,
                "echo": {
                    "session_id": request.query.get("session_id"),
                    "name": request.query.get("name"),
                    "has_name_param": "name" in request.query,
                    "bytes": len(raw),
                    "sha256": hashlib.sha256(raw).hexdigest(),
                },
            }
        )

    async def _tools(self, request: web.Request) -> web.Response:
        return web.json_response(
            {
                "tools": [
                    {
                        "ref": "Bash",
                        "namespace": "default",
                        "name": "Bash",
                        "llm_name": "Bash",
                        "description": "stub",
                    }
                ]
            }
        )

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

    async def _text(self, request: web.Request) -> web.Response:
        """非 JSON 响应（如网关 500 的 HTML 错误页）；``?fail=1`` 取 500 分支。"""
        status = 500 if "fail" in request.query else 200
        return web.Response(
            text="<html>not json</html>", content_type="text/html", status=status
        )

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
    """``Driver.resume``：workspace 从响应回填，重复挂载刷新 response。"""
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


@pytest.mark.asyncio
async def test_http_client_ignores_env_proxy(
    stub_gateway: StubGateway, monkeypatch: pytest.MonkeyPatch
) -> None:
    """loopback 直连不得被环境代理劫持（trust_env=False）。

    没有这条缝时，CI / 本机代理（含 macOS 系统代理经 ``getproxies()``）会把
    ``127.0.0.1`` 的请求也送进代理，表现为整组场景假红。
    """
    monkeypatch.setenv("HTTP_PROXY", "http://127.0.0.1:9")  # 死代理：走它必失败
    monkeypatch.setenv("http_proxy", "http://127.0.0.1:9")
    monkeypatch.setenv("ALL_PROXY", "http://127.0.0.1:9")
    http = DriverHttp(stub_gateway.url, started_at=0.0)

    response = await http.health()

    assert response == {"status": "ok"}
    assert len(http.calls) == 1
    await http.close()


@pytest.mark.asyncio
async def test_named_actions_hit_documented_endpoints(
    stub_gateway: StubGateway,
) -> None:
    """具名动作（``set_session_tools`` / ``release_session``）的 path 与请求体。

    这两个端点上游 SDK 没有（``update_session`` 缺 ``tools`` 形参、release 未包装），
    probe 用它们驱动工具热切换与逐出——形状漂移（path / 字段名）必须在这里变红，
    而不是让场景在"改了不生效"的沉默里失败。
    """
    http = DriverHttp(stub_gateway.url, started_at=0.0)

    updated = await http.set_session_tools("sid-1", ["Bash", "Read"])
    assert updated == {
        "ok": True,
        "echo": {"session_id": "sid-1", "tools": ["Bash", "Read"]},
    }, updated
    released = await http.release_session("sid-1")
    assert released == {"ok": True, "echo": {"session_id": "sid-1"}}, released

    assert [(call.method, call.path, call.body) for call in http.calls] == [
        (
            "POST",
            "/api/session/update",
            {"session_id": "sid-1", "tools": ["Bash", "Read"]},
        ),
        ("POST", "/api/session/release", {"session_id": "sid-1"}),
    ], http.calls
    await http.close()


@pytest.mark.asyncio
async def test_list_tools_reads_registry_surface(stub_gateway: StubGateway) -> None:
    """``list_tools`` 打 ``GET /api/tools``（注册表视图的具名动作）。"""
    http = DriverHttp(stub_gateway.url, started_at=0.0)

    payload = await http.list_tools()

    assert [tool["llm_name"] for tool in payload["tools"]] == ["Bash"], payload
    assert [(call.method, call.path) for call in http.calls] == [("GET", "/api/tools")]
    await http.close()


@pytest.mark.asyncio
async def test_upload_media_posts_raw_bytes(stub_gateway: StubGateway) -> None:
    """``upload_media``：字节**逐字节**到达端点，query 带 session_id / name。

    这是 03 的 raw-body 通道：请求体不是 JSON，因此它不进 ``HttpCall.body``，
    只在 ``detail`` 里留长度（现场转储不该被图片字节灌满）。
    """
    http = DriverHttp(stub_gateway.url, started_at=0.0)
    data = b"\x89PNG\r\n\x1a\nraw-upload-payload"

    response = await http.upload_media("sid-1", data, name="shot.png")

    assert response == {
        "ok": True,
        "echo": {
            "session_id": "sid-1",
            "name": "shot.png",
            "has_name_param": True,
            "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        },
    }, response
    call = http.calls[0]
    assert (call.method, call.path, call.status) == (
        "POST",
        "/api/session/media",
        200,
    )
    assert call.body is None, "原始字节体不该进 JSON 留档"
    assert call.detail == f"raw body: {len(data)} byte(s)", call.detail
    assert call.params == {"session_id": "sid-1", "name": "shot.png"}, call.params
    await http.close()


@pytest.mark.asyncio
async def test_upload_media_omits_name_when_absent(stub_gateway: StubGateway) -> None:
    """``name`` 缺省 = 不发这个 query 参数（空串与缺省在服务端同价，但不替客户端造值）。"""
    http = DriverHttp(stub_gateway.url, started_at=0.0)

    response = await http.upload_media("sid-2", b"tiny")

    assert response["echo"]["name"] is None, response
    assert response["echo"]["has_name_param"] is False, response
    assert http.calls[0].params == {"session_id": "sid-2"}, http.calls[0].params
    await http.close()


@pytest.mark.asyncio
async def test_low_level_doors_carry_raw_bytes(stub_gateway: StubGateway) -> None:
    """两个低层口子（``request`` / ``request_raw``）都能带原始字节体。

    具名动作（``upload_media``）不是唯一通道：将来测别的 raw-body 端点时，低层
    口子必须能一次到位（否则又得改驱动）。
    """
    http = DriverHttp(stub_gateway.url, started_at=0.0)
    data = b"\x89PNG-low-level-raw"

    payload = await http.request(
        "POST",
        "/api/session/media",
        content=data,
        params={"session_id": "sid-7"},
    )

    assert payload["echo"]["bytes"] == len(data), payload
    assert payload["echo"]["sha256"] == hashlib.sha256(data).hexdigest(), payload
    assert http.calls[0].detail == f"raw body: {len(data)} byte(s)"

    tiny = b"\x00\x01"
    call, raw = await http.request_raw(
        "POST", "/api/session/media", content=tiny, params={"session_id": "sid-8"}
    )
    assert call.params == {"session_id": "sid-8"}, call.params
    assert call.detail == f"raw body: {len(tiny)} byte(s)", call.detail
    assert call.response is None, "parse_json=False：响应只留原始字节"
    assert json.loads(raw)["echo"]["sha256"] == hashlib.sha256(tiny).hexdigest(), raw
    await http.close()


@pytest.mark.asyncio
async def test_raw_body_detail_survives_a_non_json_response(
    stub_gateway: StubGateway,
) -> None:
    """raw body 长度与非 JSON 响应两条留档事实**同时**保留（不互相覆盖）。

    最现实的那一支是"raw-body 请求撞上 500 的 HTML 错误页"：两条事实都在，才是
    可离线复盘的现场。
    """
    http = DriverHttp(stub_gateway.url, started_at=0.0)
    merged = "raw body: 5 byte(s); response body is not JSON"

    payload = await http.request("POST", "/api/session/text", content=b"12345")
    assert payload is None, "非 JSON 响应：低层口子原样回 None（留档才是断言面）"

    with pytest.raises(DriverHttpError):
        await http.request(
            "POST", "/api/session/text", content=b"12345", params={"fail": "1"}
        )

    assert [call.detail for call in http.calls] == [merged, merged], http.calls
    assert [call.status for call in http.calls] == [200, 500], http.calls
    assert "not json" in http.calls[0].text, http.calls[0].text
    await http.close()
