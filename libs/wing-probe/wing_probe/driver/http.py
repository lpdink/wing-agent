"""HTTP 动作封装 —— 复用 ``wing_sdk.GatewayClient`` + 全程留档（tasks 4.1）。

driver 只经公开协议驱动网关，因此 HTTP 通道直接用「系统外面」的官方客户端：
``DriverHttp`` 继承 ``wing_sdk.GatewayClient``（会话生命周期 / 查询 / 变更全部方法），
只改写它唯一的两处出网口（``_post`` / ``_get``）来做两件事：

1. **留档**：method / path / 请求体 / 状态码 / 响应体 / 相对时间——断言"网关回了什么"
   不靠重放，靠留档（``http.calls`` / ``last_call`` / ``calls_for``）；
2. **非 2xx 报错**：抛 ``DriverHttpError``，错误消息里带请求体与响应体——不允许
   "静默失败"，否则场景会以莫名其妙的下游断言失败告终。

JSON 之外的响应（畸形 body / 网关 500 的 HTML/纯文本）原样截断保留在 ``HttpCall.text``，
出错报告直接引用它。
"""

from __future__ import annotations

import json
import time
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any

import httpx
from wing_sdk.http_client import GatewayClient

#: 原始响应文本的保留上限（解析后的 JSON 全量保留；原始文本只为诊断畸形 body）。
HTTP_TEXT_LIMIT = 8 * 1024

#: 默认单请求超时（覆盖 GatewayClient 的 60s；probe 场景都在本地）。
DEFAULT_TIMEOUT = 30.0


@dataclass(frozen=True, slots=True)
class HttpCall:
    """一次 HTTP 调用的留档。"""

    method: str
    path: str
    body: dict[str, Any] | None
    status: int
    response: Any = None
    """解析后的 JSON 响应（非 JSON 响应为 None）。"""
    text: str = ""
    """原始响应文本（截断到 ``HTTP_TEXT_LIMIT``）。"""
    params: Mapping[str, Any] | None = None
    at: float = 0.0
    """相对 env 启动的单调时钟（秒）。"""
    duration_ms: float = 0.0
    detail: str | None = None
    """留档之外的补充说明（如"响应不是 JSON 对象"）。"""

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300

    @property
    def session_id(self) -> str | None:
        """请求体 / 响应体里出现的 session_id（``calls_for`` 的检索依据）。"""
        for source in (self.body, self.response):
            if isinstance(source, Mapping):
                value = source.get("session_id") or source.get("source_session_id")
                if isinstance(value, str):
                    return value
        return None

    def render(self, *, limit: int = 400) -> str:
        """一行摘要（报告与排查用）。"""
        parts = [
            f"{self.method} {self.path} → {self.status} ({self.duration_ms:.1f}ms)"
        ]
        if self.body is not None:
            parts.append(f"  request: {_truncate_json(self.body, limit)}")
        if self.response is not None:
            parts.append(f"  response: {_truncate_json(self.response, limit)}")
        elif self.text:
            parts.append(f"  response text: {_truncate(self.text, limit)}")
        if self.detail:
            parts.append(f"  detail: {self.detail}")
        return "\n".join(parts)


def _truncate(text: str, limit: int) -> str:
    return (
        text if len(text) <= limit else f"{text[:limit]}…(+{len(text) - limit} chars)"
    )


def _truncate_json(value: Any, limit: int) -> str:
    try:
        body = json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
    except (TypeError, ValueError):  # pragma: no cover - default=str 已兜底
        body = repr(value)
    return _truncate(body, limit)


class DriverHttpError(RuntimeError):
    """非 2xx 响应（或响应形态不符预期）——错误里带请求体与响应体。"""

    def __init__(self, call: HttpCall, *, detail: str | None = None) -> None:
        self.call = call
        self.status = call.status
        super().__init__(render_http_failure(call, detail=detail))


def render_http_failure(call: HttpCall, *, detail: str | None = None) -> str:
    """HTTP 失败报告（状态码 + 请求 + 响应；不含网关日志——那是 env 的职责）。"""
    line = f"{call.method} {call.path} failed with HTTP {call.status}"
    if detail:
        line += f" ({detail})"
    return f"{line}\n{call.render()}"


class DriverHttp(GatewayClient):
    """带留档的 ``GatewayClient``（driver 的 HTTP 通道）。

    用法：

        http = DriverHttp(env.gateway_url, started_at=env.started_at)
        resp = await http.create_session(workspace=str(tmp))
        http.last_call(path="/api/session/create").status == 200

    **与 ``wing_sdk`` 的耦合**：留档完全依赖上游把出网收敛到 ``_post`` / ``_get``
    这两个**私有**方法上（本类只覆盖它们）。上游若改名 / 改调用点，留档会**静默
    全空**（`http.calls` 一直是 `[]`），断言"网关回了什么"的场景随之失真。
    这不是注释能守住的，由装配层自测兜住：``tests/test_driver_http.py`` 用桩网关
    调用**公开**方法（``health()`` 等）并断言首个调用确实被留档——上游改缝即红。
    """

    def __init__(
        self,
        gateway_url: str,
        api_key: str | None = None,
        *,
        started_at: float | None = None,
        clock: Callable[[], float] = time.monotonic,
        timeout: float = DEFAULT_TIMEOUT,
        max_calls: int = 2000,
        text_limit: int = HTTP_TEXT_LIMIT,
    ) -> None:
        super().__init__(gateway_url, api_key=api_key)
        self._client.timeout = httpx.Timeout(timeout)
        self._clock = clock
        self._started_at = clock() if started_at is None else started_at
        self._calls: list[HttpCall] = []
        self._max_calls = max_calls
        self._text_limit = text_limit

    # ── 留档 ──────────────────────────────────────────────────

    @property
    def calls(self) -> list[HttpCall]:
        """全部调用（按发生顺序）。"""
        return list(self._calls)

    def last_call(
        self, *, path: str | None = None, method: str | None = None
    ) -> HttpCall | None:
        """最近一次调用（可按 path / method 过滤；没有则 None）。"""
        for call in reversed(self._calls):
            if path is not None and call.path != path:
                continue
            if method is not None and call.method != method.upper():
                continue
            return call
        return None

    def calls_for(self, session_id: str) -> list[HttpCall]:
        """与某 session 相关的调用（请求体或响应体里出现该 id）。"""
        return [call for call in self._calls if call.session_id == session_id]

    def find(self, path: str, *, method: str | None = None) -> list[HttpCall]:
        """按 path（精确匹配）检索全部调用。"""
        return [
            call
            for call in self._calls
            if call.path == path and (method is None or call.method == method.upper())
        ]

    def clear(self) -> None:
        """清空留档（场景内分段断言时用）。"""
        self._calls.clear()

    def _record(self, call: HttpCall) -> HttpCall:
        self._calls.append(call)
        if len(self._calls) > self._max_calls:
            del self._calls[0 : len(self._calls) - self._max_calls]
        return call

    # ── 出网口（GatewayClient 唯一的两处） ────────────────────

    async def _post(
        self, path: str, body: dict, headers: dict[str, str] | None = None
    ) -> dict:
        call, payload = await self._call("POST", path, body=body, headers=headers)
        return self._as_object(call, payload)

    async def _get(self, path: str, params: dict | None = None) -> dict:
        call, payload = await self._call("GET", path, params=params)
        return self._as_object(call, payload)

    async def request(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        params: Mapping[str, Any] | None = None,
        headers: Mapping[str, str] | None = None,
    ) -> Any:
        """低层口子：任意端点的留档调用（非 2xx 抛 ``DriverHttpError``）。"""
        _, payload = await self._call(
            method, path, body=body, params=params, headers=headers
        )
        return payload

    async def _call(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        params: Mapping[str, Any] | None = None,
        headers: Mapping[str, str] | None = None,
    ) -> tuple[HttpCall, Any]:
        """执行一次请求：留档（无论成败）→ 非 2xx 抛错。"""
        started = self._clock()
        at = started - self._started_at
        response = await self._client.request(
            method.upper(),
            path,
            json=body,
            params=dict(params) if params is not None else None,
            headers=dict(headers) if headers is not None else None,
        )
        duration_ms = (self._clock() - started) * 1000.0
        payload: Any = None
        detail: str | None = None
        try:
            payload = response.json()
        except ValueError:
            detail = "response body is not JSON"
        call = self._record(
            HttpCall(
                method=method.upper(),
                path=path,
                body=body if isinstance(body, dict) else None,
                status=response.status_code,
                response=payload,
                text=_truncate(response.text, self._text_limit),
                params=dict(params) if params is not None else None,
                at=at,
                duration_ms=duration_ms,
                detail=detail,
            )
        )
        if not call.ok:
            raise DriverHttpError(call)
        return call, payload

    # ── 内部 ──────────────────────────────────────────────────

    @staticmethod
    def _as_object(call: HttpCall, payload: Any) -> dict:
        """``GatewayClient`` 的方法约定返回 JSON 对象——形状不符即显式失败。"""
        if isinstance(payload, dict):
            return payload
        raise DriverHttpError(
            call, detail=f"expected a JSON object, got {type(payload).__name__}"
        )
