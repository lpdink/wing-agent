"""tests/test_gateway_wake.py — 唤醒端点的错误映射与 wire 形状。

用 ASGI 直连（``httpx.ASGITransport``，与测试同线程/同事件循环）+ **真实** ``WakeService``
（假 ports / 注入库）：路由只是薄壳，要证明的是 HTTP 面的契约——200 的形状（含 ISO 时间）、
404（会话/id 不存在）、400（非法参数）、503（``wing.db`` 降级）。

为什么不用 ``TestClient``：它把请求跑在**另一个线程**里，而 ``wing.db`` 的连接归属单线程
（``check_same_thread=True``，产品语义就是这样）。ASGI 直连保住了同线程前提。
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

from collections.abc import AsyncIterator

import httpx
import pytest
import pytest_asyncio

from wing.db.database import Database
from wing.event_bus import event_bus
from wing.gateway.app import create_app
from wing.wake import WakeService

pytestmark = pytest.mark.asyncio

CHILD = "20260101-120000-child"
PARENT = "20260101-120000-parent"


class FakePorts:
    def __init__(self) -> None:
        self.existing = {CHILD, PARENT}
        self.archived: set[str] = set()
        self.delivered: list[tuple[str, str, str]] = []

    def session_exists(self, session_id: str) -> bool:
        return session_id in self.existing

    def is_archived(self, session_id: str) -> bool:
        return session_id in self.archived

    async def deliver(self, *, session_id: str, content: str, request_id: str) -> None:
        self.delivered.append((session_id, content, request_id))


def _client(wake: WakeService) -> httpx.AsyncClient:
    from wing.config import AuthConfig

    server = MagicMock()
    server.runtime.wake = wake
    server.host = "127.0.0.1"
    server.cors_origins = []
    # 正常模式（否则 SetupGuardMiddleware 对白名单外的路径一律 503）且鉴权关闭
    server.in_setup_mode = False
    server.auth_config = AuthConfig()
    app = create_app(server)
    return httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://testserver"
    )


@pytest.fixture(autouse=True)
def clean_event_bus():
    """EventBus 是全局单例：每个测试前后清空订阅（``install()`` 会往上挂 handler）。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


@pytest.fixture
def ports() -> FakePorts:
    return FakePorts()


@pytest.fixture
def wake(tmp_path: Path, ports: FakePorts) -> WakeService:
    service = WakeService(ports, database=Database(tmp_path / "wing.db"))
    service.install()
    return service


@pytest_asyncio.fixture
async def client(wake: WakeService) -> AsyncIterator[httpx.AsyncClient]:
    async with _client(wake) as http:
        yield http


def _session_body(**overrides):
    body = {
        "kind": "session",
        "target_session_id": PARENT,
        "source_session_id": CHILD,
    }
    body.update(overrides)
    return body


class TestCreate:
    async def test_returns_the_wire_shape(self, client: httpx.AsyncClient) -> None:
        resp = await client.post("/api/wake/create", json=_session_body())
        assert resp.status_code == 200, resp.text
        wake = resp.json()["wake"]
        assert set(wake) == {
            "id",
            "kind",
            "target_session_id",
            "source_session_id",
            "on_events",
            "spec",
            "prompt",
            "state",
            "fire_count",
            "last_fire_at",
            "last_error",
            "created_at",
            "next_fire_at",
        }
        assert wake["kind"] == "session"
        assert wake["target_session_id"] == PARENT
        assert wake["on_events"] == "done,ask"
        assert wake["state"] == "active"
        assert wake["fire_count"] == 0
        assert wake["spec"] is None and wake["next_fire_at"] is None
        assert wake["created_at"]  # 本地 ISO 字符串
        assert "T" in wake["created_at"]

    async def test_timer_gets_a_next_fire(self, client: httpx.AsyncClient) -> None:
        resp = await client.post(
            "/api/wake/create",
            json={
                "kind": "timer",
                "target_session_id": PARENT,
                "spec": "every:30m",
                "prompt": "ping",
            },
        )
        assert resp.status_code == 200, resp.text
        wake = resp.json()["wake"]
        assert wake["spec"] == "every:30m"
        assert wake["next_fire_at"] and wake["next_fire_at"] > wake["created_at"]

    async def test_missing_session_is_404(self, client: httpx.AsyncClient) -> None:
        resp = await client.post(
            "/api/wake/create", json=_session_body(source_session_id="ghost")
        )
        assert resp.status_code == 404
        assert "ghost" in resp.json()["detail"]

    async def test_self_wake_is_400(self, client: httpx.AsyncClient) -> None:
        resp = await client.post(
            "/api/wake/create", json=_session_body(target_session_id=CHILD)
        )
        assert resp.status_code == 400
        assert "must differ" in resp.json()["detail"]

    async def test_bad_spec_is_400(self, client: httpx.AsyncClient) -> None:
        resp = await client.post(
            "/api/wake/create",
            json={
                "kind": "timer",
                "target_session_id": PARENT,
                "spec": "every:soon",
                "prompt": "ping",
            },
        )
        assert resp.status_code == 400
        assert "duration" in resp.json()["detail"]

    async def test_one_shot_with_times_is_400(self, client: httpx.AsyncClient) -> None:
        resp = await client.post(
            "/api/wake/create",
            json={
                "kind": "timer",
                "target_session_id": PARENT,
                "spec": "in:5m",
                "prompt": "ping",
                "times": 3,
            },
        )
        assert resp.status_code == 400
        assert "times" in resp.json()["detail"]

    async def test_unknown_kind_is_422(self, client: httpx.AsyncClient) -> None:
        """kind 是 Literal：schema 层面就拒（422 是 FastAPI 的既有形状）。"""
        resp = await client.post(
            "/api/wake/create",
            json={"kind": "cron", "target_session_id": PARENT},
        )
        assert resp.status_code == 422


class TestList:
    async def test_active_only_by_default(self, client: httpx.AsyncClient) -> None:
        first = (await client.post("/api/wake/create", json=_session_body())).json()[
            "wake"
        ]
        # 一条会 404 的创建：脏数据不进门
        await client.post(
            "/api/wake/create", json=_session_body(source_session_id="ghost")
        )
        await client.post("/api/wake/cancel", json={"id": first["id"]})

        assert (await client.get("/api/wake/list")).json() == {"wakes": []}
        everything = (await client.get("/api/wake/list", params={"all": "true"})).json()
        assert [w["id"] for w in everything["wakes"]] == [first["id"]]
        assert everything["wakes"][0]["state"] == "canceled"


class TestCancel:
    async def test_cancel_then_idempotent(self, client: httpx.AsyncClient) -> None:
        wake = (await client.post("/api/wake/create", json=_session_body())).json()[
            "wake"
        ]
        first = await client.post("/api/wake/cancel", json={"id": wake["id"]})
        assert first.status_code == 200
        assert first.json() == {"ok": True, "canceled": True}
        again = await client.post("/api/wake/cancel", json={"id": wake["id"]})
        assert again.json() == {"ok": True, "canceled": False}

    async def test_unknown_id_is_404(self, client: httpx.AsyncClient) -> None:
        resp = await client.post("/api/wake/cancel", json={"id": 4242})
        assert resp.status_code == 404
        assert "4242" in resp.json()["detail"]


class TestDegradedDatabase:
    async def test_create_is_503(self, tmp_path: Path, ports: FakePorts) -> None:
        path = tmp_path / "wing.db"
        path.mkdir()
        service = WakeService(ports, database=Database(path))
        service.install()
        async with _client(service) as client:
            resp = await client.post("/api/wake/create", json=_session_body())
        assert resp.status_code == 503
        assert "unavailable" in resp.json()["detail"]

    async def test_list_is_empty(self, tmp_path: Path, ports: FakePorts) -> None:
        path = tmp_path / "wing.db"
        path.mkdir()
        async with _client(WakeService(ports, database=Database(path))) as client:
            assert (await client.get("/api/wake/list")).json() == {"wakes": []}
