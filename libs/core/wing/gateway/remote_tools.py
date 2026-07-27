# wing/gateway/remote_tools.py — 远程工具管理器

"""RemoteToolManager —— Gateway 侧远程工具的连接与调用中枢。

职责（全部为 Gateway/网络层概念，核心 runtime 不感知）：
  - 维护 tool host 连接：client_id → WebSocket
  - 注册远程工具：为每个工具构造一个绑定 client_id 的 dispatch 闭包，
    以 namespace=client_id 注入核心 tool_registry。核心只看到一个普通
    Tool（schema + callable），对其远程性一无所知——这是本设计的关键。
  - 调用分发：dispatch 闭包生成 call_id，经 WS 发 ToolCallRequest，
    在 pending future 表上等待 ToolCallResult resolve。
  - 生命周期：宽口径总超时兜底；断连时 fail 所有在途调用并注销工具。

调用往返完全落在本 manager 的 future 表里，不经过 EventBus——EventBus
只负责出站事件通知，不承担请求/响应 RPC。
"""

from __future__ import annotations

import asyncio
import uuid
from typing import Any

from wing.common.logger import log
from wing.config import get_config
from wing.schema import Tool, ToolError
from wing.tool_registry import tool_registry

from .protocol import RemoteToolSpec, ToolCallRequest


class RemoteToolManager:
    """远程工具连接与调用管理器（Gateway 持有单例）。

    内部索引：
      - _clients:         client_id → WebSocket
      - _pending:         call_id → Future[str]（全局，resolve 据此 O(1) 寻址）
      - _client_calls:    client_id → set[call_id]（断连批量 fail + 归属校验）
      - _send_locks:      client_id → Lock（串行化对同一 WS 的并发写）
      - _receives_events: client_id → bool（tool_runtime 为 False，不收事件）
    """

    def __init__(self, timeout: float | None = None) -> None:
        # timeout=None 时每次 dispatch 从 config 读 remote_tool_timeout，
        # 便于热重载；测试可注入固定值。
        self._timeout = timeout
        self._clients: dict[str, Any] = {}
        self._pending: dict[str, asyncio.Future[str]] = {}
        self._client_calls: dict[str, set[str]] = {}
        self._send_locks: dict[str, asyncio.Lock] = {}
        self._receives_events: dict[str, bool] = {}

    @property
    def _effective_timeout(self) -> float:
        if self._timeout is not None:
            return self._timeout
        return get_config().gateway.remote_tool_timeout

    # ── 连接管理 ──────────────────────────────────────────────

    def attach(self, client_id: str, ws: Any, *, receives_events: bool = True) -> None:
        """登记一个 tool host 连接。

        receives_events=False 用于 tool_runtime——纯工具执行远端，不参与
        事件订阅（global 广播会跳过它）。
        """
        self._clients[client_id] = ws
        self._client_calls.setdefault(client_id, set())
        self._send_locks.setdefault(client_id, asyncio.Lock())
        self._receives_events[client_id] = receives_events
        log.info(
            f"RemoteToolManager: attached tool host '{client_id}' "
            f"(receives_events={receives_events})"
        )

    def is_attached(self, client_id: str) -> bool:
        return client_id in self._clients

    def receives_events(self, client_id: str) -> bool:
        """该 client 是否接收事件。未 attach 的 client（纯前端）默认接收。"""
        return self._receives_events.get(client_id, True)

    # ── 注册 ──────────────────────────────────────────────────

    def register_tools(self, client_id: str, specs: list[RemoteToolSpec]) -> list[str]:
        """把 tool host 的工具以 namespace=client_id 注入核心 registry。

        每个工具的 function 是一个绑定 (client_id, tool_name) 的 dispatch
        闭包——Agent 调用它时内部发起远程调用。返回完整工具引用列表。

        原子性：先校验（请求内重名 + registry 碰撞）再提交，要么全成要么
        全不成——碰撞时不留部分注册（spec 碰撞契约：既有工具保持不变）。

        Raises:
            ValueError: 请求内工具重名，或同 namespace 同名工具已存在。
        """
        # 1. 请求内查重
        names = [spec.name for spec in specs]
        if len(names) != len(set(names)):
            dupes = sorted({n for n in names if names.count(n) > 1})
            raise ValueError(f"duplicate tool names in request: {dupes}")

        # 2. registry 碰撞预检（提交前）
        for spec in specs:
            if tool_registry.get_tool(spec.name, client_id) is not None:
                raise ValueError(
                    f"Tool '{spec.name}' already registered in namespace '{client_id}'"
                )

        # 3. 提交（预检通过后不会碰撞）
        registered: list[str] = []
        for spec in specs:
            tool = Tool(
                name=spec.name,
                namespace=client_id,
                description=spec.description,
                params=list(spec.params),
                function=self._make_dispatch(client_id, spec.name),
            )
            tool_registry.register_tool(tool)
            registered.append(f"{client_id}.{spec.name}")
        log.info(
            f"RemoteToolManager: registered {len(registered)} tools for '{client_id}': "
            f"{registered}"
        )
        return registered

    # ── 调用分发 ──────────────────────────────────────────────

    def _make_dispatch(self, client_id: str, tool_name: str) -> Any:
        """构造远程工具的 dispatch 闭包（作为 Tool.function 注入核心）。

        闭包签名用 **arguments 以触发 agent 的全量透传路径；参数集合由
        远程 schema 完整定义，本闭包不解释参数，原样转发给 tool host。
        """

        async def dispatch(**arguments: Any) -> str:
            return await self._dispatch(client_id, tool_name, arguments)

        return dispatch

    async def _dispatch(self, client_id: str, tool_name: str, arguments: dict) -> str:
        ws = self._clients.get(client_id)
        if ws is None:
            raise ToolError(
                f"remote tool '{tool_name}' unavailable: client '{client_id}' "
                f"is not connected"
            )

        # 读一次超时，wait_for 与错误消息共用，避免热重载下两次读取不一致
        timeout = self._effective_timeout

        call_id = uuid.uuid4().hex
        loop = asyncio.get_running_loop()
        future: asyncio.Future[str] = loop.create_future()
        self._pending[call_id] = future
        self._client_calls.setdefault(client_id, set()).add(call_id)

        request = ToolCallRequest(call_id=call_id, name=tool_name, arguments=arguments)
        # per-client 锁串行化对同一 WS 的并发写——agent 并发执行多个远程工具
        # （asyncio.gather）时，避免对单连接并发 send（ASGI 不保证安全）。
        lock = self._send_locks.setdefault(client_id, asyncio.Lock())
        try:
            async with lock:
                await ws.send_text(request.model_dump_json())
        except Exception as e:
            self._discard_call(client_id, call_id)
            raise ToolError(
                f"remote tool '{tool_name}' failed to dispatch: connection error: {e}"
            ) from e

        try:
            return await asyncio.wait_for(future, timeout=timeout)
        except asyncio.TimeoutError:
            self._discard_call(client_id, call_id)
            raise ToolError(
                f"remote tool '{tool_name}' timed out after {timeout:.0f}s"
            ) from None

    def resolve_result(
        self, client_id: str, call_id: str, result: str, is_error: bool
    ) -> bool:
        """resolve 一个在途调用。返回是否命中已知 call_id。

        归属校验：call_id 必须属于发帧的 client——防止 tool host A 用伪造
        结果 resolve host B 的在途调用（多 host 隔离）。call_id 是 uuid 不可
        猜，但归属校验闭合了 RBAC 主题下的这道边界。

        is_error 时以 ToolError 置错——dispatch 侧 wait_for 会抛出它，
        agent 据此把结果标记为工具错误。
        """
        calls = self._client_calls.get(client_id)
        if calls is None or call_id not in calls:
            return False
        future = self._pending.pop(call_id, None)
        if future is None or future.done():
            return False
        calls.discard(call_id)
        if is_error:
            future.set_exception(ToolError(result or "remote tool error"))
        else:
            future.set_result(result)
        return True

    # ── 断连清理 ──────────────────────────────────────────────

    def fail_client(self, client_id: str, reason: str) -> None:
        """断连时：fail 该 client 所有在途调用 + 注销其全部远程工具。

        断连是首要失败信号——在途调用立即以错误结束，模型敏锐感知；
        工具从核心 registry 移除，后续新建 agent 不再看到它们。

        KV Cache 保护（重要）：本方法**只**清理全局 tool_registry，绝不
        触碰已加载 agent 的 ``_tool_map``。修改 agent 已绑定的工具集会破坏
        KV cache。已绑定进 agent 的 dispatch 闭包保持原样——当 agent 再次
        调用它时，``_dispatch`` 在调用时刻检测到 client 不在线，返回清晰的
        "tool unavailable / not connected" 错误作为工具结果。

        动态工具切换（掉线时往 latest message 插入一条"某工具已掉线"以在
        不破坏 KV cache 的前提下通知 agent）是后续特性，不在本实现范围。
        """
        call_ids = list(self._client_calls.pop(client_id, set()))
        for call_id in call_ids:
            future = self._pending.pop(call_id, None)
            if future is not None and not future.done():
                future.set_exception(ToolError(f"remote tool call aborted: {reason}"))
        self._clients.pop(client_id, None)
        self._send_locks.pop(client_id, None)
        self._receives_events.pop(client_id, None)

        removed = tool_registry.unregister_namespace(client_id)
        log.info(
            f"RemoteToolManager: client '{client_id}' disconnected ({reason}); "
            f"failed {len(call_ids)} pending calls, unregistered {len(removed)} tools"
        )

    def _discard_call(self, client_id: str, call_id: str) -> None:
        """清除单个 call_id 的索引（超时 / 发送失败时）。"""
        self._pending.pop(call_id, None)
        if client_id in self._client_calls:
            self._client_calls[client_id].discard(call_id)
