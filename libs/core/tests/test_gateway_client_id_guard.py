"""`X-Client-Id` 缺失时的线格式：三个端点都是 **400 + ``missing X-Client-Id header``**。

这条 400 是客户端可见的 ``detail``（CLI / tool host 按它判断"我漏了 header"），
依赖函数住在 ``gateway/routes/_common.py`` 之后仍必须逐字不变——改文案等于改协议。
本文件只钉这三条路径的**状态码 + detail 整串**。
"""

from __future__ import annotations

from collections.abc import Iterator
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from wing.config import ApiKeyEntry, AuthConfig

EXPECTED = "missing X-Client-Id header"

#: 三个用 ``Depends(require_client_id)`` 的端点及其最小请求体。
GUARDED = {
    "POST /api/session/subscribe": (
        "/api/session/subscribe",
        {"session_id": "s"},
    ),
    "POST /api/session/unsubscribe": (
        "/api/session/unsubscribe",
        {"session_id": "s"},
    ),
    "POST /api/tools/register": (
        "/api/tools/register",
        {"tools": []},
    ),
}


def _mock_config() -> MagicMock:
    config = MagicMock()
    config.gateway.auth = AuthConfig(
        enabled=False, keys=[ApiKeyEntry(key="k", role="admin")]
    )
    return config


@pytest.fixture
def client() -> Iterator[TestClient]:
    with (
        patch("wing.gateway.server.WingRuntime") as MockRuntime,
        patch("wing.gateway.server.load_config") as mock_load_config,
    ):
        MockRuntime.return_value = MagicMock()
        mock_load_config.return_value = _mock_config()
        from wing.gateway.server import GatewayServer

        server = GatewayServer()
        runtime = MagicMock()
        # lifespan 的 shutdown 会 await runtime.wake.stop()（无 async 替身会炸 teardown）。
        runtime.wake.stop = AsyncMock()
        server.runtime = runtime
        with TestClient(server._app) as tc:
            yield tc


@pytest.mark.parametrize("case", GUARDED, ids=list(GUARDED))
def test_missing_client_id_is_a_400_with_the_wire_detail(case: str, client: TestClient):
    path, body = GUARDED[case]
    resp = client.post(path, json=body)
    assert resp.status_code == 400
    assert resp.json()["detail"] == EXPECTED


@pytest.mark.parametrize("case", GUARDED, ids=list(GUARDED))
def test_header_present_changes_the_outcome(case: str, client: TestClient):
    """对照：带上 header 后不再是这条 400（证明上面那条确实是 header 闸门给的）。"""
    path, body = GUARDED[case]
    resp = client.post(path, json=body, headers={"X-Client-Id": "someone"})
    assert resp.json().get("detail") != EXPECTED
