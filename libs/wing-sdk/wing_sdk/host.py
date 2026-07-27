"""ToolHost — 远程工具宿主。

装饰器注册 + WS 连接 + HTTP 注册 + 调用服务循环。
"""

from __future__ import annotations

import asyncio
import inspect
import json
import re
from typing import Any, Callable

import httpx
import websockets

from wing_sdk.schema import RemoteToolSpec, ToolParam


class ConnectionClosed(Exception):
    """WS 连接关闭。"""


class ToolHost:
    """远程工具宿主——注册工具并服务调用。

    Usage:
        host = ToolHost(client_id="my-host", gateway_url="http://127.0.0.1:32523")

        @host.tool(name="Bash")
        async def bash(command: str, timeout: int = 30) -> str:
            '''Execute a shell command.

            Args:
                command: The command to execute.
                timeout: Maximum wait time in seconds.
            '''
            ...

        await host.run()
    """

    def __init__(
        self,
        client_id: str,
        gateway_url: str = "http://127.0.0.1:32523",
        api_key: str | None = None,
    ) -> None:
        self.client_id = client_id
        self.gateway_url = gateway_url.rstrip("/")
        self.api_key = api_key
        self._tools: dict[str, _ToolEntry] = {}
        self.ready = asyncio.Event()
        """注册完成后 set——runner 可 await 此事件确保工具已就绪。"""

    def tool(
        self,
        name: str | None = None,
        description: str | None = None,
        params: list[ToolParam] | None = None,
        llm_name: str | None = None,
    ) -> Callable:
        """装饰器——将 async 函数注册为远程工具。

        从函数签名推断参数（名称、类型、默认值），从 docstring 的
        Google-style Args section 提取参数描述。显式传入的参数覆盖推断。
        """

        def decorator(fn: Callable) -> Callable:
            tool_name = name or getattr(fn, "__name__", "")
            tool_desc = description or _extract_description(fn)
            tool_params = params or _infer_params(fn)

            spec = RemoteToolSpec(
                name=tool_name,
                description=tool_desc,
                llm_name=llm_name,
                params=tool_params,
            )
            self._tools[tool_name] = _ToolEntry(spec=spec, handler=fn)
            return fn

        return decorator

    def register_spec(self, spec: RemoteToolSpec, handler: Callable) -> None:
        """直接注册一个已构造好的工具规格（非装饰器方式）。"""
        self._tools[spec.name] = _ToolEntry(spec=spec, handler=handler)

    @property
    def specs(self) -> list[RemoteToolSpec]:
        return [entry.spec for entry in self._tools.values()]

    async def run(self) -> None:
        """连接 Gateway、注册工具、进入调用服务循环。

        WS 断连时抛出 ConnectionClosed。
        """
        ws_url = self.gateway_url.replace("http://", "ws://").replace(
            "https://", "wss://"
        )
        ws_uri = f"{ws_url}/ws?client_id={self.client_id}"

        headers: dict[str, str] = {}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"

        async with websockets.connect(ws_uri, additional_headers=headers) as ws:
            # 1. 读 ConnectResponse
            first = await ws.recv()
            msg = json.loads(first)
            if msg.get("type") != "connected":
                raise ConnectionClosed(f"unexpected first frame: {first}")

            # 2. HTTP 注册工具
            await self._register_tools_http()
            self.ready.set()

            # 3. 服务循环
            await self._serve(ws)

    async def _register_tools_http(self) -> None:
        headers: dict[str, str] = {"X-Client-Id": self.client_id}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"

        body = {"tools": [spec.to_dict() for spec in self.specs]}
        async with httpx.AsyncClient() as client:
            resp = await client.post(
                f"{self.gateway_url}/api/tools/register",
                json=body,
                headers=headers,
            )
            if resp.status_code != 200:
                raise ConnectionClosed(
                    f"tool registration failed ({resp.status_code}): {resp.text}"
                )

    async def _serve(self, ws: Any) -> None:
        """读循环：收 tool_call_request → dispatch → 回 tool_call_result。"""
        tasks: set[asyncio.Task] = set()  # type: ignore[type-arg]
        try:
            async for raw in ws:
                try:
                    payload = json.loads(raw)
                except json.JSONDecodeError:
                    continue

                if payload.get("type") != "tool_call_request":
                    continue

                call_id = payload.get("call_id", "")
                tool_name = payload.get("name", "")
                arguments = payload.get("arguments", {})

                task = asyncio.create_task(
                    self._dispatch(ws, call_id, tool_name, arguments)
                )
                tasks.add(task)
                task.add_done_callback(tasks.discard)
        except websockets.exceptions.ConnectionClosed as e:
            raise ConnectionClosed(f"WebSocket closed: {e}") from e
        finally:
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)

        # async for 正常结束 = 对端关闭（ConnectionClosedOK 不抛异常）
        raise ConnectionClosed("WebSocket connection closed by gateway")

    async def _dispatch(
        self, ws: Any, call_id: str, tool_name: str, arguments: dict
    ) -> None:
        entry = self._tools.get(tool_name)
        if entry is None:
            await self._send_result(ws, call_id, f"unknown tool: {tool_name}", True)
            return

        try:
            result = await entry.handler(**arguments)
            await self._send_result(ws, call_id, str(result), False)
        except Exception as e:
            await self._send_result(ws, call_id, str(e), True)

    async def _send_result(
        self, ws: Any, call_id: str, result: str, is_error: bool
    ) -> None:
        frame = {
            "type": "tool_call_result",
            "call_id": call_id,
            "result": result,
            "is_error": is_error,
        }
        await ws.send(json.dumps(frame))


# ── 内部 ──────────────────────────────────────────────────────


class _ToolEntry:
    __slots__ = ("spec", "handler")

    def __init__(self, spec: RemoteToolSpec, handler: Callable) -> None:
        self.spec = spec
        self.handler = handler


_TYPE_MAP: dict[type, str] = {
    str: "string",
    int: "integer",
    float: "number",
    bool: "boolean",
    list: "array",
    dict: "object",
}


def _hint_to_type(hint: object) -> tuple[str, str | None]:
    """将 type hint 转为 (param_type, items)。处理泛型和 Optional。"""
    import typing
    from types import UnionType

    if hint is None or hint is inspect.Parameter.empty:
        return ("string", None)

    # 直接基本类型
    if isinstance(hint, type) and hint in _TYPE_MAP:
        return (_TYPE_MAP[hint], None)

    origin = typing.get_origin(hint)

    # list[str] / list[int]
    if origin is list:
        args = typing.get_args(hint)
        items = _TYPE_MAP.get(args[0], "string") if args else "string"
        return ("array", items)

    # Optional[X] / X | None — unwrap to inner type
    if origin is typing.Union or isinstance(hint, UnionType):
        args = [a for a in typing.get_args(hint) if a is not type(None)]
        if args:
            return _hint_to_type(args[0])
        return ("string", None)

    return ("string", None)


def _extract_description(fn: Callable) -> str:
    """从 docstring 提取首段作为工具描述。"""
    doc = inspect.getdoc(fn) or ""
    # 首段 = 到第一个空行或 Args: 之前
    lines: list[str] = []
    for line in doc.split("\n"):
        stripped = line.strip()
        if stripped.lower().startswith("args:") or stripped == "" and lines:
            break
        if stripped:
            lines.append(stripped)
    return " ".join(lines)


def _infer_params(fn: Callable) -> list[ToolParam]:
    """从函数签名 + docstring Args section 推断参数列表。"""
    import typing

    sig = inspect.signature(fn)
    try:
        hints = typing.get_type_hints(fn)
    except Exception:
        hints = {}
    arg_docs = _parse_args_section(inspect.getdoc(fn) or "")

    params: list[ToolParam] = []
    for param_name, param in sig.parameters.items():
        if param_name in ("self", "cls"):
            continue

        hint = hints.get(param_name)
        param_type, items = _hint_to_type(hint)

        default = None
        if param.default is not inspect.Parameter.empty:
            default = param.default

        params.append(
            ToolParam(
                name=param_name,
                type=param_type,
                description=arg_docs.get(param_name, ""),
                default=default,
                items=items,
            )
        )
    return params


def _parse_args_section(doc: str) -> dict[str, str]:
    """解析 Google-style docstring 的 Args section。

    格式：
        Args:
            name: description
            other: description
                continuation line
    """
    result: dict[str, str] = {}
    in_args = False
    current_name: str | None = None

    for line in doc.split("\n"):
        stripped = line.strip()

        if stripped.lower().startswith("args:"):
            in_args = True
            continue

        if in_args:
            # 新 section 开始（Returns:, Raises:, 等）
            if stripped and stripped.endswith(":") and not stripped.startswith(" "):
                if re.match(r"^[A-Z]\w*:", stripped):
                    break

            # 参数行：name: description
            match = re.match(r"^(\w+)\s*(?:\([^)]*\))?\s*:\s*(.*)", stripped)
            if match:
                current_name = match.group(1)
                result[current_name] = match.group(2).strip()
            elif current_name and stripped:
                # 续行
                result[current_name] += " " + stripped

    return result
