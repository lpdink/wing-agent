"""ToolHost 连接构造测试。"""

import pytest

import wing_sdk.host as host_mod
from wing_sdk.host import ToolHost

pytestmark = pytest.mark.asyncio


def _patch_connect(monkeypatch, captured: dict):
    """替换 websockets.connect，捕获 URI 后立即终止 run()。"""

    class _FakeConnect:
        def __init__(self, uri, **kwargs):
            captured["uri"] = uri

        async def __aenter__(self):
            raise RuntimeError("stop")

        async def __aexit__(self, *args):
            return False

    monkeypatch.setattr(host_mod.websockets, "connect", _FakeConnect)


async def test_run_url_encodes_client_id(monkeypatch):
    captured: dict = {}
    _patch_connect(monkeypatch, captured)

    host = ToolHost(client_id="my host&x=1")
    with pytest.raises(RuntimeError, match="stop"):
        await host.run()

    # 空格 / & / = 被编码，不会截断或破坏 query
    assert "client_id=my%20host%26x%3D1" in captured["uri"]


async def test_run_safe_client_id_unchanged(monkeypatch):
    captured: dict = {}
    _patch_connect(monkeypatch, captured)

    host = ToolHost(client_id="wing-orch-abcd1234")
    with pytest.raises(RuntimeError, match="stop"):
        await host.run()

    # 字母数字与 - 保持原样（自动生成的 ID 不受影响）
    assert "client_id=wing-orch-abcd1234" in captured["uri"]
