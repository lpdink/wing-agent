"""最小 tool host —— 远程工具宿主的探针侧实现（走公开 HTTP / WS 协议）。

为什么在 probe 里自己实现（而不是复用 ``wing_sdk.ToolHost``）：``wing_sdk`` 是**产品
SDK**，是"外部宿主"的一种参考实现；probe 需要的是*测试控制面*——留档每次调用、按需
挂起不应答、主动断开连接、检查注册响应——这些都不该反向加进产品 SDK。本模块与
probe 其它部分同纪律：**不得 import wing**（AST 门禁），只经公开协议说话：

1. WS 连接 ``/ws?client_id=<id>``（首帧必须是 ``ConnectResponse``；声明 client_id 的
   连接即具备注册资格，见 ``docs/dev/http-api.md``）；
2. ``POST /api/tools/register``（``X-Client-Id`` header）注册工具规格；
3. 服务循环：收 ``tool_call_request`` 帧 → 执行 handler → 回 ``tool_call_result``
   帧（同 ``call_id``）。

用法（场景侧）::

    host = ToolHost("probe-host", probe.env.gateway_url, started_at=probe.env.started_at)
    host.add_tool("Echo", handler=echo, params=[{"name": "token", "type": "string"}])
    await host.start()
    ...
    await host.wait_for_call("Echo", timeout=10)   # 确认在途
    await host.close()                             # 模拟宿主进程死亡

``calls`` 是**收到过**的调用留档（``at`` 相对 ``started_at``，与 probe 时间线同尺度）；
``sent_results`` 是**回过**的结果帧留档。``close()`` 幂等且不抛——失败场景的收尾也要安全。
"""

from __future__ import annotations

import asyncio
import inspect
import json
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any
from urllib.parse import quote

import httpx
from websockets.asyncio.client import ClientConnection
from websockets.asyncio.client import connect as ws_connect
from websockets.exceptions import ConnectionClosed

from wing_probe.driver import DEFAULT_MAX_SIZE, ws_url

#: 出站/入站帧类型（与 ``gateway/protocol/system.py`` 的 ToolCallRequest / ToolCallResult 对齐）。
REQUEST_TYPE = "tool_call_request"
RESULT_TYPE = "tool_call_result"

#: 注册端点的 path（``routes/tools.py``）。
REGISTER_PATH = "/api/tools/register"

DEFAULT_OPEN_TIMEOUT = 10.0
DEFAULT_CLOSE_TIMEOUT = 5.0
DEFAULT_WAIT_TIMEOUT = 10.0
DEFAULT_HTTP_TIMEOUT = 10.0

Handler = Callable[..., Any]
"""工具 handler：``handler(**arguments)``，sync / async 均可，返回值 ``str()`` 后回传。"""


class ToolHostError(RuntimeError):
    """tool host 握手 / 注册 / 用法错误。"""


@dataclass(frozen=True, slots=True)
class RemoteTool:
    """一个远程工具规格 + 本地 handler（注册 payload 的事实来源）。"""

    name: str
    handler: Handler
    description: str = ""
    params: tuple[Mapping[str, Any], ...] = ()
    llm_name: str | None = None

    def to_wire(self) -> dict[str, Any]:
        """注册请求里的单个工具规格（字段名与 ``RemoteToolSpec`` 一致）。"""
        spec: dict[str, Any] = {
            "name": self.name,
            "description": self.description,
            "params": [dict(param) for param in self.params],
        }
        if self.llm_name is not None:
            spec["llm_name"] = self.llm_name
        return spec


@dataclass(frozen=True, slots=True)
class RemoteCall:
    """tool host 收到的一次调用（``at`` 相对 ``started_at`` 的秒数）。"""

    call_id: str
    name: str
    arguments: dict[str, Any]
    at: float


@dataclass(slots=True)
class _SentResult:
    call_id: str
    result: str
    is_error: bool


class ToolHost:
    """一个远程工具宿主：连接、注册、服务调用。

    生命周期：``start()``（连接 + 注册 + 起服务任务）→ 任意次调用往返 →
    ``close()``（取消未完成 dispatch + 关 WS；幂等）。``start()`` 之后不再支持
    重连——场景需要"宿主动态"就用两个 host。
    """

    def __init__(
        self,
        client_id: str,
        gateway_url: str,
        *,
        api_key: str | None = None,
        started_at: float | None = None,
        max_size: int = DEFAULT_MAX_SIZE,
        open_timeout: float = DEFAULT_OPEN_TIMEOUT,
        close_timeout: float = DEFAULT_CLOSE_TIMEOUT,
        http_timeout: float = DEFAULT_HTTP_TIMEOUT,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.client_id = client_id
        self.gateway_url = gateway_url.rstrip("/")
        self.api_key = api_key
        self._max_size = max_size
        self._open_timeout = open_timeout
        self._close_timeout = close_timeout
        self._http_timeout = http_timeout
        self._clock = clock
        self._started_at = clock() if started_at is None else started_at

        self._tools: dict[str, RemoteTool] = {}
        self._calls: list[RemoteCall] = []
        self._results: list[_SentResult] = []
        self._registered: list[str] = []
        self._ws: ClientConnection | None = None
        self._serve_task: asyncio.Task[None] | None = None
        self._tasks: set[asyncio.Task[None]] = set()
        self._closing = False
        """``close()`` 的幂等闸门（一旦进入收尾就不再重复）。"""
        self._closed = False
        """连接已不可用（服务循环退出或已 close）。"""
        self._condition = asyncio.Condition()

    # ── 构造 ──────────────────────────────────────────────────

    def add_tool(
        self,
        name: str,
        handler: Handler,
        *,
        description: str = "",
        params: Sequence[Mapping[str, Any]] = (),
        llm_name: str | None = None,
    ) -> RemoteTool:
        """登记一个工具（重复名 / ``start()`` 之后调用抛 ``ToolHostError``）。"""
        if not name:
            raise ToolHostError("tool name must not be empty")
        if self._ws is not None:
            raise ToolHostError("add_tool() must be called before start()")
        if name in self._tools:
            raise ToolHostError(f"tool {name!r} is already registered on this host")
        tool = RemoteTool(
            name=name,
            handler=handler,
            description=description,
            params=tuple(dict(param) for param in params),
            llm_name=llm_name,
        )
        self._tools[name] = tool
        return tool

    # ── 状态 ──────────────────────────────────────────────────

    @property
    def ws_url(self) -> str:
        """带 ``client_id`` 的 WS 地址（client_id 做百分号编码）。"""
        return f"{ws_url(self.gateway_url)}?client_id={quote(self.client_id, safe='')}"

    @property
    def connected(self) -> bool:
        return self._ws is not None and not self._closed

    @property
    def registered(self) -> list[str]:
        """注册响应的 ``registered``（完整 ref 列表；未注册时为空）。"""
        return list(self._registered)

    @property
    def calls(self) -> list[RemoteCall]:
        """收到过的全部调用（按到达顺序）。"""
        return list(self._calls)

    @property
    def sent_results(self) -> list[dict[str, Any]]:
        """回过的全部结果帧（``tool_call_result`` 的线上形状）。"""
        return [
            {
                "type": RESULT_TYPE,
                "call_id": entry.call_id,
                "result": entry.result,
                "is_error": entry.is_error,
            }
            for entry in self._results
        ]

    def calls_named(self, name: str) -> list[RemoteCall]:
        """按工具名过滤调用留档。"""
        return [call for call in self._calls if call.name == name]

    def now(self) -> float:
        """当前时刻（相对 ``started_at`` 的秒数）。"""
        return self._clock() - self._started_at

    # ── 生命周期 ──────────────────────────────────────────────

    async def start(self) -> ToolHost:
        """连接网关、注册工具、启动服务循环（失败时自行收尾再抛）。"""
        if self._ws is not None:
            raise ToolHostError("ToolHost.start() called twice")
        try:
            ws = await ws_connect(
                self.ws_url,
                max_size=self._max_size,
                open_timeout=self._open_timeout,
                close_timeout=self._close_timeout,
                additional_headers=(
                    {"Authorization": f"Bearer {self.api_key}"}
                    if self.api_key
                    else None
                ),
                # 只连 loopback：绕开环境 / 系统代理（与 driver 同款信任边界）。
                proxy=None,
            )
        except Exception as exc:
            raise ToolHostError(f"failed to connect to {self.ws_url}: {exc!r}") from exc
        self._ws = ws
        try:
            first = await asyncio.wait_for(ws.recv(), timeout=self._open_timeout)
            data = json.loads(first if isinstance(first, str) else first.decode())
            if data.get("type") != "connected":
                raise ToolHostError(f"unexpected first frame: {first!r}")
            await self._register_tools()
        except BaseException:
            await self.close()
            raise
        self._serve_task = asyncio.create_task(self._serve())
        return self

    async def close(self) -> None:
        """断开连接并取消未完成的 dispatch（幂等、不抛）。"""
        if self._closing:
            return
        self._closing = True
        self._closed = True
        for task in list(self._tasks):
            task.cancel()
        if self._tasks:
            await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()
        serve_task, self._serve_task = self._serve_task, None
        if serve_task is not None:
            serve_task.cancel()
            await asyncio.gather(serve_task, return_exceptions=True)
        ws, self._ws = self._ws, None
        if ws is not None:
            try:
                await ws.close()
            except Exception:  # pragma: no cover - 关闭失败无需补救
                pass

    async def wait_for_call(
        self,
        name: str | None = None,
        *,
        timeout: float = DEFAULT_WAIT_TIMEOUT,
    ) -> RemoteCall:
        """等到至少一条匹配的调用留档，返回**最新**一条。

        幂等：调用早于本方法到达也不漏（判据是"留档里存在"，不是"新到达"）。
        """
        if not isinstance(timeout, (int, float)) or timeout <= 0:
            raise ToolHostError(f"timeout must be a positive number, got {timeout!r}")

        def match(call: RemoteCall) -> bool:
            return name is None or call.name == name

        async def find() -> RemoteCall | None:
            for call in reversed(self._calls):
                if match(call):
                    return call
            return None

        async with self._condition:
            existing = await find()
            if existing is not None:
                return existing
            try:
                await asyncio.wait_for(
                    self._condition.wait_for(lambda: any(map(match, self._calls))),
                    timeout,
                )
            except TimeoutError as exc:
                raise ToolHostError(
                    f"no {'matching ' if name else ''}tool call within {timeout:.1f}s; "
                    f"recorded calls: {[call.name for call in self._calls]}"
                ) from exc
            found = await find()
            assert found is not None  # wait_for 的谓词保证
            return found

    # ── 注册 ──────────────────────────────────────────────────

    async def _register_tools(self) -> None:
        if not self._tools:
            raise ToolHostError("no tools registered on this host")
        headers = {"X-Client-Id": self.client_id}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        body = {"tools": [tool.to_wire() for tool in self._tools.values()]}
        async with httpx.AsyncClient(
            base_url=self.gateway_url,
            timeout=httpx.Timeout(self._http_timeout),
            trust_env=False,
        ) as client:
            try:
                response = await client.post(REGISTER_PATH, json=body, headers=headers)
            except httpx.HTTPError as exc:
                raise ToolHostError(
                    f"tool registration request failed: {exc!r}"
                ) from exc
        if response.status_code != 200:
            raise ToolHostError(
                f"tool registration failed ({response.status_code}): {response.text}"
            )
        payload = response.json()
        registered = payload.get("registered") if isinstance(payload, dict) else None
        self._registered = (
            [str(item) for item in registered] if isinstance(registered, list) else []
        )

    # ── 服务循环 ──────────────────────────────────────────────

    async def _serve(self) -> None:
        ws = self._ws
        assert ws is not None
        try:
            async for raw in ws:
                text = raw if isinstance(raw, str) else raw.decode("utf-8", "replace")
                try:
                    payload = json.loads(text)
                except ValueError:
                    continue
                if not isinstance(payload, dict) or payload.get("type") != REQUEST_TYPE:
                    continue
                call = RemoteCall(
                    call_id=str(payload.get("call_id", "")),
                    name=str(payload.get("name", "")),
                    arguments=dict(payload.get("arguments") or {}),
                    at=self.now(),
                )
                self._calls.append(call)
                async with self._condition:
                    self._condition.notify_all()
                task = asyncio.create_task(self._dispatch(call))
                self._tasks.add(task)
                task.add_done_callback(self._tasks.discard)
        except ConnectionClosed:
            pass
        except asyncio.CancelledError:
            raise
        finally:
            self._closed = True

    async def _dispatch(self, call: RemoteCall) -> None:
        tool = self._tools.get(call.name)
        if tool is None:
            await self._send_result(
                call.call_id, f"unknown tool: {call.name}", is_error=True
            )
            return
        try:
            result = tool.handler(**call.arguments)
            if inspect.isawaitable(result):
                result = await result
            await self._send_result(call.call_id, str(result), is_error=False)
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            await self._send_result(call.call_id, str(exc), is_error=True)

    async def _send_result(self, call_id: str, result: str, *, is_error: bool) -> None:
        ws = self._ws
        if ws is None or self._closed:
            return
        frame = {
            "type": RESULT_TYPE,
            "call_id": call_id,
            "result": result,
            "is_error": is_error,
        }
        try:
            await ws.send(json.dumps(frame, ensure_ascii=False))
        except ConnectionClosed:
            return
        self._results.append(
            _SentResult(call_id=call_id, result=result, is_error=is_error)
        )


__all__ = [
    "DEFAULT_CLOSE_TIMEOUT",
    "DEFAULT_OPEN_TIMEOUT",
    "DEFAULT_WAIT_TIMEOUT",
    "REGISTER_PATH",
    "REQUEST_TYPE",
    "RESULT_TYPE",
    "RemoteCall",
    "RemoteTool",
    "ToolHost",
    "ToolHostError",
]
