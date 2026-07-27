"""RemoteToolManager 单元测试——远程工具的连接、调用分发与生命周期。

不启动真实 server / WS：用 FakeWS 捕获出站帧，manager 的 future 表完成
请求/响应关联。这些测试不依赖 docker，进 `make test-python`。
"""

from __future__ import annotations

import asyncio
import json

import pytest

from wing.gateway.protocol import RemoteToolSpec
from wing.gateway.remote_tools import RemoteToolManager
from wing.schema import ToolError
from wing.tool_registry import tool_registry

HOST = "test-host"


class FakeWS:
    """捕获出站帧的假 WebSocket。"""

    def __init__(self) -> None:
        self.sent: list[dict] = []

    async def send_text(self, data: str) -> None:
        self.sent.append(json.loads(data))

    def last_call_id(self) -> str:
        return self.sent[-1]["call_id"]


@pytest.fixture
def manager():
    """每个测试一个 manager；结束后注销其 namespace，避免污染全局 registry。"""
    mgr = RemoteToolManager(timeout=5.0)
    yield mgr
    tool_registry.unregister_namespace(HOST)


def _spec(name: str) -> RemoteToolSpec:
    return RemoteToolSpec(name=name, description=f"remote {name}", params=[])


@pytest.mark.asyncio
async def test_register_and_dispatch_round_trip(manager: RemoteToolManager):
    ws = FakeWS()
    manager.attach(HOST, ws)
    registered = manager.register_tools(HOST, [_spec("Read")])
    assert registered == [f"{HOST}.Read"]

    tool = tool_registry.resolve(f"{HOST}.Read")
    assert tool is not None

    task = asyncio.create_task(tool.function(path="/etc/hostname"))
    while not ws.sent:  # 等出站帧落地
        await asyncio.sleep(0)

    frame = ws.sent[-1]
    assert frame["type"] == "tool_call_request"
    assert frame["name"] == "Read"
    assert frame["arguments"] == {"path": "/etc/hostname"}

    assert manager.resolve_result(frame["call_id"], "file content", False) is True
    assert await task == "file content"


@pytest.mark.asyncio
async def test_dispatch_error_result_raises_tool_error(manager: RemoteToolManager):
    ws = FakeWS()
    manager.attach(HOST, ws)
    manager.register_tools(HOST, [_spec("Bash")])
    tool = tool_registry.resolve(f"{HOST}.Bash")
    assert tool is not None

    task = asyncio.create_task(tool.function(command="false"))
    while not ws.sent:
        await asyncio.sleep(0)

    manager.resolve_result(ws.last_call_id(), "exit code 1", True)
    with pytest.raises(ToolError, match="exit code 1"):
        await task


@pytest.mark.asyncio
async def test_dispatch_times_out(manager: RemoteToolManager):
    manager._timeout = 0.05  # 收紧超时以加速测试
    ws = FakeWS()
    manager.attach(HOST, ws)
    manager.register_tools(HOST, [_spec("Bash")])
    tool = tool_registry.resolve(f"{HOST}.Bash")
    assert tool is not None

    with pytest.raises(ToolError, match="timed out"):
        await tool.function(command="sleep 100")


@pytest.mark.asyncio
async def test_fail_client_aborts_pending_and_unregisters(manager: RemoteToolManager):
    ws = FakeWS()
    manager.attach(HOST, ws)
    manager.register_tools(HOST, [_spec("Read"), _spec("Write")])
    tool = tool_registry.resolve(f"{HOST}.Read")
    assert tool is not None

    task = asyncio.create_task(tool.function(path="/x"))
    while not ws.sent:
        await asyncio.sleep(0)

    manager.fail_client(HOST, "connection closed")

    with pytest.raises(ToolError, match="connection closed"):
        await task
    # 工具已注销
    assert tool_registry.resolve(f"{HOST}.Read") is None
    assert tool_registry.resolve(f"{HOST}.Write") is None
    assert not manager.is_attached(HOST)


@pytest.mark.asyncio
async def test_dispatch_to_unattached_client_errors(manager: RemoteToolManager):
    # 注册时不要求 attach，但调用时 client 必须在线
    manager.register_tools(HOST, [_spec("Read")])
    tool = tool_registry.resolve(f"{HOST}.Read")
    assert tool is not None

    with pytest.raises(ToolError, match="not connected"):
        await tool.function(path="/x")


def test_resolve_unknown_call_id_returns_false(manager: RemoteToolManager):
    assert manager.resolve_result("nonexistent", "x", False) is False


@pytest.mark.asyncio
async def test_held_tool_reference_survives_disconnect_with_clear_error(
    manager: RemoteToolManager,
):
    """KV cache 保护契约：断连只清 registry，不动 agent 持有的工具引用。

    模拟 agent 已绑定某远程工具（持有其 function 引用）。tool host 断连后，
    该引用仍可调用，且清晰返回 "not connected" 错误——而非破坏 agent 工具集。
    """
    ws = FakeWS()
    manager.attach(HOST, ws)
    manager.register_tools(HOST, [_spec("Read")])

    # agent 持有的绑定引用（断连前取得）
    held = tool_registry.resolve(f"{HOST}.Read")
    assert held is not None
    held_fn = held.function

    manager.fail_client(HOST, "connection closed")

    # registry 已注销（新 agent 看不到），但持有的引用仍可用并给出清晰错误
    assert tool_registry.resolve(f"{HOST}.Read") is None
    with pytest.raises(ToolError, match="not connected"):
        await held_fn(path="/x")
