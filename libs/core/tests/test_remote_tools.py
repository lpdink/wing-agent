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


class TestRemoteToolSpecValidation:
    """工具名校验——含 '.' 或空串会破坏 ToolRef 解析，注册时即拒绝。"""

    def test_name_with_dot_rejected(self):
        with pytest.raises(ValueError, match="must not contain"):
            RemoteToolSpec(name="my.tool", description="", params=[])

    def test_empty_name_rejected(self):
        with pytest.raises(ValueError, match="must not be empty"):
            RemoteToolSpec(name="   ", description="", params=[])

    def test_valid_name_ok(self):
        assert RemoteToolSpec(name="Read", description="", params=[]).name == "Read"

    def test_name_whitespace_rejected(self):
        with pytest.raises(ValueError, match="whitespace"):
            RemoteToolSpec(name="  Read", description="", params=[])

    def test_llm_name_bad_charset_rejected(self):
        # 点号不符合 provider function-name 文法
        with pytest.raises(ValueError, match="function-name grammar"):
            RemoteToolSpec(name="Read", llm_name="host.Read", params=[])

    def test_llm_name_none_ok(self):
        assert RemoteToolSpec(name="Read", llm_name=None).llm_name is None

    def test_llm_name_valid_ok(self):
        spec = RemoteToolSpec(name="Read", llm_name="Remote-Read_1", params=[])
        assert spec.llm_name == "Remote-Read_1"

    def test_bare_name_internal_space_rejected(self):
        # llm_name=None 时裸 name 即 LLM 可见名，"My Tool" 不合 provider 文法
        with pytest.raises(ValueError, match="function-name grammar"):
            RemoteToolSpec(name="My Tool", llm_name=None, params=[])

    def test_bare_name_non_ascii_rejected(self):
        with pytest.raises(ValueError, match="function-name grammar"):
            RemoteToolSpec(name="读文件", llm_name=None, params=[])

    def test_non_ascii_name_with_safe_llm_name_ok(self):
        # name 仅作 registry key（不直达 provider），llm_name 安全即可
        spec = RemoteToolSpec(name="读文件", llm_name="Read", params=[])
        assert spec.name == "读文件"


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

    assert manager.resolve_result(HOST, frame["call_id"], "file content", False) is True
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

    manager.resolve_result(HOST, ws.last_call_id(), "exit code 1", True)
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
async def test_dispatch_cancellation_cleans_up(manager: RemoteToolManager):
    """等待中的 dispatch 被取消（interrupt/shutdown 场景）时清理索引，无残留。"""
    ws = FakeWS()
    manager.attach(HOST, ws)
    manager.register_tools(HOST, [_spec("Bash")])
    tool = tool_registry.resolve(f"{HOST}.Bash")
    assert tool is not None

    task = asyncio.create_task(tool.function(command="sleep 100"))
    while not ws.sent:
        await asyncio.sleep(0)
    call_id = ws.last_call_id()
    assert call_id in manager._client_calls[HOST]
    assert call_id in manager._pending

    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task

    # finally 清理：pending 与 client_calls 都不留残留
    assert call_id not in manager._pending
    assert call_id not in manager._client_calls.get(HOST, set())


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
    # 未知 client → False
    assert manager.resolve_result("ghost-client", "some-call", "", False) is False


@pytest.mark.asyncio
async def test_resolve_result_rejects_cross_client_forgery(manager: RemoteToolManager):
    """归属校验：host A 不能用伪造结果 resolve host B 的在途调用。"""
    other = "other-host"
    ws_a, ws_b = FakeWS(), FakeWS()
    manager.attach(HOST, ws_a)
    manager.attach(other, ws_b)
    manager.register_tools(other, [_spec("Read")])
    tool = tool_registry.resolve(f"{other}.Read")
    assert tool is not None

    task = asyncio.create_task(tool.function(path="/x"))
    while not ws_b.sent:
        await asyncio.sleep(0)
    stolen_call_id = ws_b.last_call_id()

    try:
        # HOST 试图 resolve other 的调用 → 拒绝
        assert manager.resolve_result(HOST, stolen_call_id, "forged", False) is False
        # 调用仍在途；真正的 owner 可以 resolve
        assert manager.resolve_result(other, stolen_call_id, "real", False) is True
        assert await task == "real"
    finally:
        tool_registry.unregister_namespace(other)


def test_register_tools_atomic_on_registry_collision(manager: RemoteToolManager):
    """碰撞时批量注册原子回滚——前序项不留残留。"""
    manager.register_tools(HOST, [_spec("Read")])  # 先占用 Read

    with pytest.raises(ValueError, match="already registered"):
        manager.register_tools(HOST, [_spec("Write"), _spec("Read")])

    # Write 未被部分注册
    assert tool_registry.resolve(f"{HOST}.Write") is None
    assert tool_registry.resolve(f"{HOST}.Read") is not None


def test_register_tools_rejects_intra_request_duplicate(manager: RemoteToolManager):
    """同一请求内工具重名 → 拒绝，且无任何注册。"""
    with pytest.raises(ValueError, match="duplicate tool names"):
        manager.register_tools(HOST, [_spec("Read"), _spec("Read")])
    assert tool_registry.resolve(f"{HOST}.Read") is None


def test_register_tools_with_llm_name(manager: RemoteToolManager):
    """端上声明的 llm_name 透传到 Tool，决定 LLM 可见名。"""
    manager.attach(HOST, FakeWS())
    spec = RemoteToolSpec(
        name="Read", description="d", llm_name="RemoteRead", params=[]
    )
    manager.register_tools(HOST, [spec])
    tool = tool_registry.resolve(f"{HOST}.Read")
    assert tool is not None
    assert tool.effective_llm_name == "RemoteRead"


def test_register_tools_default_llm_name_is_bare(manager: RemoteToolManager):
    """不声明 llm_name 时退化为裸 name（运行时不自动以 client_id 限定）。"""
    manager.attach(HOST, FakeWS())
    manager.register_tools(HOST, [_spec("Read")])
    tool = tool_registry.resolve(f"{HOST}.Read")
    assert tool is not None
    assert tool.effective_llm_name == "Read"


@pytest.mark.asyncio
async def test_concurrent_dispatch_serialized_by_lock(manager: RemoteToolManager):
    """同一 host 并发 dispatch（agent asyncio.gather 场景）都成功——per-client
    锁串行化对单连接的并发写，两个调用各自正确 resolve。"""
    ws = FakeWS()
    manager.attach(HOST, ws)
    manager.register_tools(HOST, [_spec("Read"), _spec("Glob")])
    read = tool_registry.resolve(f"{HOST}.Read")
    glob = tool_registry.resolve(f"{HOST}.Glob")
    assert read is not None and glob is not None

    t_read = asyncio.create_task(read.function(path="/a"))
    t_glob = asyncio.create_task(glob.function(pattern="*"))
    while len(ws.sent) < 2:
        await asyncio.sleep(0)

    # 按帧顺序 resolve（call_id → 工具名）
    for frame in ws.sent:
        result = "R" if frame["name"] == "Read" else "G"
        manager.resolve_result(HOST, frame["call_id"], result, False)

    assert await t_read == "R"
    assert await t_glob == "G"


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
