"""ToolHost — 连接握手、工具注册与调用服务循环。

`requirements`：run() 的失败语义是「抛 `ConnectionClosed`」——宿主进程据此决定
是否重连；单测用替身把 WS / HTTP 两条腿都换成可编程的假件。
"""

from __future__ import annotations

import json
from typing import Any

import pytest
import websockets

import wing_sdk.host as host_mod
from wing_sdk.host import ConnectionClosed, ToolHost

pytestmark = pytest.mark.asyncio


# ── 替身 ──────────────────────────────────────────────────────


class _FakeWS:
    """最小 WS 替身：recv 出首帧（ConnectResponse），异步迭代出后续帧，send 记录出帧。"""

    def __init__(self, first: str, frames: list[str], error: Exception | None = None):
        self._first = first
        self._frames = frames
        self._error = error
        self.sent: list[dict] = []

    async def recv(self) -> str:
        return self._first

    async def send(self, data: str) -> None:
        self.sent.append(json.loads(data))

    async def _iter(self):
        for frame in self._frames:
            yield frame
        if self._error is not None:
            raise self._error

    def __aiter__(self):
        return self._iter()


class _FakeConnect:
    """websockets.connect 替身：记录 URI / kwargs，进入上下文返回给定 ws。"""

    def __init__(self, ws: _FakeWS, captured: dict):
        self._ws = ws
        self._captured = captured

    def __call__(self, uri: str, **kwargs: Any) -> "_FakeConnect":
        self._captured["uri"] = uri
        self._captured["kwargs"] = kwargs
        return self

    async def __aenter__(self) -> _FakeWS:
        return self._ws

    async def __aexit__(self, *args: Any) -> bool:
        return False


class _FakeHTTPResponse:
    def __init__(self, status_code: int) -> None:
        self.status_code = status_code
        self.text = "boom"


class _FakeHTTPClient:
    """httpx.AsyncClient 替身：记录 /api/tools/register 的请求体。"""

    def __init__(self, captured: dict, status_code: int = 200) -> None:
        self._captured = captured
        self._status_code = status_code

    def __call__(self, *args: Any, **kwargs: Any) -> "_FakeHTTPClient":
        return self

    async def __aenter__(self) -> "_FakeHTTPClient":
        return self

    async def __aexit__(self, *args: Any) -> bool:
        return False

    async def post(self, url: str, json: Any = None, headers: Any = None):
        self._captured["post"] = {"url": url, "json": json, "headers": headers}
        return _FakeHTTPResponse(self._status_code)


def _patch(monkeypatch, ws: _FakeWS, status_code: int = 200) -> dict:
    captured: dict = {}
    monkeypatch.setattr(host_mod.websockets, "connect", _FakeConnect(ws, captured))
    monkeypatch.setattr(
        host_mod.httpx, "AsyncClient", _FakeHTTPClient(captured, status_code)
    )
    return captured


def _frame(payload: dict) -> str:
    return json.dumps(payload)


def _tool_call(call_id: str = "call-1", name: str = "Echo", **arguments) -> str:
    return _frame(
        {
            "type": "tool_call_request",
            "call_id": call_id,
            "name": name,
            "arguments": arguments or {"text": "hi"},
        }
    )


# ── 工具注册（装饰器 / 显式）──────────────────────────────────


async def test_decorator_infers_spec_from_signature_and_docstring():
    host = ToolHost(client_id="h1")

    @host.tool()
    async def echo(text: str, times: int = 2, tags: list[str] | None = None) -> str:
        """Echo the text back.

        Args:
            text: What to echo.
            times: How many times.
            tags: Optional labels.

        Returns:
            The echoed text.
        """
        return text * times

    (spec,) = host.specs
    assert spec.name == "echo"
    assert spec.description == "Echo the text back."
    assert spec.llm_name is None
    assert [p.name for p in spec.params] == ["text", "times", "tags"]
    assert [p.type for p in spec.params] == ["string", "integer", "array"]
    assert [p.default for p in spec.params] == [None, 2, None]
    # 数组元素类型来自 list[str] 的元素标注
    assert spec.params[2].items == "string"
    assert [p.description for p in spec.params] == [
        "What to echo.",
        "How many times.",
        "Optional labels.",
    ]


async def test_decorator_explicit_arguments_override_inference():
    from wing_sdk.schema import ToolParam

    host = ToolHost(client_id="h1")
    explicit = [ToolParam(name="whatever", type="string", description="explicit param")]

    @host.tool(
        name="Renamed",
        description="explicit description",
        llm_name="LLM",
        params=explicit,
    )
    async def inner(whatever: str) -> str:
        """Inferred description."""
        return whatever

    (spec,) = host.specs
    assert (spec.name, spec.llm_name, spec.description) == (
        "Renamed",
        "LLM",
        "explicit description",
    )
    assert spec.params == explicit
    # 装饰器返回原函数，注册不等于包装
    assert await inner("x") == "x"


async def test_decorator_returns_registering_decorator():
    host = ToolHost(client_id="h1")

    @host.tool()
    async def f() -> str:
        return "x"

    assert callable(f)
    assert [s.name for s in host.specs] == ["f"]


async def test_register_spec_programmatic():
    from wing_sdk.schema import RemoteToolSpec

    host = ToolHost(client_id="h1")

    async def handler(**kwargs: Any) -> str:
        return "ran"

    host.register_spec(RemoteToolSpec(name="Manual"), handler)

    assert [s.name for s in host.specs] == ["Manual"]


async def test_registration_order_and_overwrite():
    host = ToolHost(client_id="h1")

    @host.tool(name="A")
    async def a() -> str:
        return "a"

    @host.tool(name="B")
    async def b() -> str:
        return "b"

    @host.tool(name="A")
    async def a2() -> str:
        return "a2"

    # dict 保序：A 的位置不动，值被覆盖
    assert [s.name for s in host.specs] == ["A", "B"]
    assert await a2() == "a2"


# ── run()：握手 + 注册 + 服务循环 ─────────────────────────────


async def test_run_registers_tools_over_http_then_serves(monkeypatch):
    host = ToolHost(client_id="orchestrator-1", gateway_url="http://127.0.0.1:32523/")
    host.register_spec(
        host_mod.RemoteToolSpec(name="Echo", description="d", llm_name="EchoLLM"),
        lambda **kw: "never",
    )
    ws = _FakeWS(first=_frame({"type": "connected"}), frames=[_tool_call()])
    captured = _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed, match="closed by gateway"):
        await host.run()

    assert captured["uri"] == "ws://127.0.0.1:32523/ws?client_id=orchestrator-1"
    assert captured["kwargs"]["max_size"] == host_mod.WS_MAX_SIZE
    assert captured["kwargs"]["additional_headers"] == {}
    assert captured["post"] == {
        "url": "http://127.0.0.1:32523/api/tools/register",
        "json": {
            "tools": [{"name": "Echo", "description": "d", "llm_name": "EchoLLM"}]
        },
        "headers": {"X-Client-Id": "orchestrator-1"},
    }
    assert host.ready.is_set()


async def test_run_rejects_connection_without_connected_frame(monkeypatch):
    host = ToolHost(client_id="h1")
    ws = _FakeWS(first=_frame({"type": "event"}), frames=[])
    captured = _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed, match="unexpected first frame"):
        await host.run()

    # 握手失败不发生工具注册，runner 可安全重连
    assert "post" not in captured


async def test_run_raises_when_registration_fails(monkeypatch):
    host = ToolHost(client_id="h1")
    ws = _FakeWS(first=_frame({"type": "connected"}), frames=[])
    _patch(monkeypatch, ws, status_code=500)

    with pytest.raises(
        ConnectionClosed, match=r"tool registration failed \(500\): boom"
    ):
        await host.run()

    assert not host.ready.is_set()


async def test_run_sends_api_key_on_both_legs(monkeypatch):
    host = ToolHost(client_id="h1", api_key="sk-9")
    ws = _FakeWS(first=_frame({"type": "connected"}), frames=[])
    captured = _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed):
        await host.run()

    assert captured["kwargs"]["additional_headers"] == {"Authorization": "Bearer sk-9"}
    assert captured["post"]["headers"]["Authorization"] == "Bearer sk-9"


async def test_run_wraps_websockets_connection_closed(monkeypatch):
    host = ToolHost(client_id="h1")
    ws = _FakeWS(
        first=_frame({"type": "connected"}),
        frames=[],
        error=websockets.exceptions.ConnectionClosed(None, None),
    )
    _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed, match="WebSocket closed"):
        await host.run()


# ── _serve：调用分发 ─────────────────────────────────────────


async def test_serve_dispatches_registered_tool(monkeypatch):
    host = ToolHost(client_id="h1")
    seen: list[dict] = []

    @host.tool(name="Echo")
    async def echo(text: str) -> str:
        seen.append({"text": text})
        return f"echo:{text}"

    ws = _FakeWS(
        first=_frame({"type": "connected"}),
        frames=[_tool_call(call_id="c-9", text="hi")],
    )
    _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed):
        await host.run()

    assert seen == [{"text": "hi"}]
    assert ws.sent == [
        {
            "type": "tool_call_result",
            "call_id": "c-9",
            "result": "echo:hi",
            "is_error": False,
        }
    ]


async def test_serve_reports_unknown_tool_as_error(monkeypatch):
    host = ToolHost(client_id="h1")
    ws = _FakeWS(
        first=_frame({"type": "connected"}),
        frames=[_tool_call(call_id="c-1", name="Nope")],
    )
    _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed):
        await host.run()

    assert ws.sent[0]["is_error"] is True
    assert ws.sent[0]["result"] == "unknown tool: Nope"


async def test_serve_turns_handler_exception_into_error_frame(monkeypatch):
    host = ToolHost(client_id="h1")

    @host.tool(name="Boom")
    async def boom(**kwargs: Any) -> str:
        raise RuntimeError("kaboom")

    ws = _FakeWS(
        first=_frame({"type": "connected"}),
        frames=[_tool_call(call_id="c-2", name="Boom", arguments={})],
    )
    _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed):
        await host.run()

    assert ws.sent == [
        {
            "type": "tool_call_result",
            "call_id": "c-2",
            "result": "kaboom",
            "is_error": True,
        }
    ]


async def test_serve_skips_malformed_and_unrelated_frames(monkeypatch):
    host = ToolHost(client_id="h1")

    @host.tool(name="Echo")
    async def echo(**kwargs: Any) -> str:
        return "ok"

    ws = _FakeWS(
        first=_frame({"type": "connected"}),
        frames=[
            "not json at all",
            _frame({"type": "some_event"}),
            json.dumps(["also", "not", "a", "dict"]),  # 合法 JSON 但非对象
            _tool_call(call_id="c-3", name="Echo", arguments={}),
        ],
    )
    _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed):
        await host.run()

    assert [f["call_id"] for f in ws.sent] == ["c-3"]


async def test_serve_defaults_missing_call_fields(monkeypatch):
    host = ToolHost(client_id="h1")

    @host.tool(name="Echo")
    async def echo(**kwargs: Any) -> str:
        return str(kwargs)

    ws = _FakeWS(
        first=_frame({"type": "connected"}),
        frames=[_frame({"type": "tool_call_request"})],
    )
    _patch(monkeypatch, ws)

    with pytest.raises(ConnectionClosed):
        await host.run()

    # 缺 call_id/name/arguments 时退化到默认值，走「未知工具」错误帧而不是崩掉
    assert ws.sent == [
        {
            "type": "tool_call_result",
            "call_id": "",
            "result": "unknown tool: ",
            "is_error": True,
        }
    ]


# ── 参数推断：type hint → LLM 工具 schema ─────────────────────


async def test_hint_to_type_mapping():
    from wing_sdk.host import _hint_to_type

    class Custom:
        pass

    assert _hint_to_type(None) == ("string", None)
    assert _hint_to_type(inspect_empty()) == ("string", None)
    assert _hint_to_type(str) == ("string", None)
    assert _hint_to_type(int) == ("integer", None)
    assert _hint_to_type(float) == ("number", None)
    assert _hint_to_type(bool) == ("boolean", None)
    assert _hint_to_type(list) == ("array", None)
    assert _hint_to_type(dict) == ("object", None)
    assert _hint_to_type(list[str]) == ("array", "string")
    assert _hint_to_type(list[int]) == ("array", "integer")
    assert _hint_to_type(list[set]) == ("array", "string")  # 未映射元素类型 → string
    assert _hint_to_type(dict[str, Any]) == ("object", None)
    assert _hint_to_type(int | None) == ("integer", None)
    assert _hint_to_type(str | int | None) == ("string", None)  # 取第一个非 None 分支
    assert _hint_to_type(Custom) == ("string", None)


def inspect_empty():
    import inspect

    return inspect.Parameter.empty


async def test_infer_params_falls_back_when_hints_unresolvable(monkeypatch):
    """typing.get_type_hints 失败（前向引用解析不到 / 坏注解）→ 退化为无 hint 推断。"""
    import typing

    host = ToolHost(client_id="h1")

    @host.tool(name="Forward")
    async def forward(x: str) -> str:
        """Doc.

        Args:
            x: whatever.
        """
        return str(x)

    def _unresolvable(fn: object) -> dict:
        raise NameError("annotation not resolvable")

    monkeypatch.setattr(typing, "get_type_hints", _unresolvable)

    (spec,) = host.specs
    assert (spec.params[0].type, spec.params[0].description) == (
        "string",
        "whatever.",
    )
    assert await forward("x") == "x"  # 注解解析不了不影响函数本身


async def test_args_section_stops_at_next_section():
    """Args 段落遇到下一个 section（Returns:）即结束，续行并入上一条描述。"""
    host = ToolHost(client_id="h1")

    @host.tool(name="Doc")
    async def doc(a: str, b: str, url: str) -> str:
        """Summary line.

        Args:
            a: first line
                continued here
            b: second
            url: endpoint, e.g. http://host:

        Returns:
            not a param: really not
        """
        return a + b + url

    (spec,) = host.specs
    assert spec.description == "Summary line."
    # 描述以 ':' 结尾的参数行不算「新 section」（只有 UpperCamel: 才算）
    assert {p.name: p.description for p in spec.params} == {
        "a": "first line continued here",
        "b": "second",
        "url": "endpoint, e.g. http://host:",
    }


async def test_extract_description_stops_at_args_without_blank_line():
    from wing_sdk.host import _extract_description

    async def fn(  # type: ignore[no-untyped-def]
        x: str,
    ) -> str:
        """One liner.

        Args:
            x: value.
        """
        return x

    assert _extract_description(fn) == "One liner."
