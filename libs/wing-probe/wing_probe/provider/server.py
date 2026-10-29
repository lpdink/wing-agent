"""假 Provider —— 进程内 aiohttp 应用（设计与 io 都不依赖 wing）。

实现 OpenAI 兼容公开协议子集：

- ``POST /v1/chat/completions``：``stream=true`` 走 SSE（见 ``sse.py``），
  ``stream=false`` 返回完整 message JSON（**压缩调用走非流式**，必须支持）；
- ``POST /<prefix>/v1/chat/completions``（``path_prefixes`` 给定的每个前缀）：
  同一条处理链的第二入口——**附加 provider**（跨 provider 场景）的 base_url 指
  到这里，「这次调用打到哪个 provider」由请求留档的 ``path`` 判定。

远端模型发现（``GET /v1/models``）已退役：网关的模型目录来自配置声明，假
Provider 不再需要应答模型列表。

剧本按 model 名路由（``ScriptRegistry``），每次入站请求消费一个 Turn；
未注册 / 剧本耗尽 → 5xx + 可读报告（含模型名与已消费/总 Turn 数）。
请求一律先留档（``RequestLog``）再路由——连"打错模型"的请求也留档。

刻意不用 uvicorn / FastAPI：假 Provider 是**同进程**对象
与测试进程共享事件循环，断言可以同步读它的状态。
"""

from __future__ import annotations

import asyncio
import socket
import time
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Any

from aiohttp import web

from wing_probe.provider.request_log import RequestLog
from wing_probe.provider.script import (
    Script,
    ScriptError,
    ScriptRegistry,
    Turn,
)
from wing_probe.provider.sse import (
    SSE_CONTENT_TYPE,
    SSEFrame,
    completion_response,
    stream_frames,
)

CHAT_PATH = "/v1/chat/completions"


@dataclass(frozen=True, slots=True)
class FrameEmit:
    """一帧被交给传输层之前的时刻（``FakeProvider.frame_emits`` 的条目）。

    ``request_index`` 是 ``RequestLog`` 留档的全局 ``index``（不是 ``model_index``）。
    ``frame_index`` 从 0 起、只数 ``data: {json}`` 载荷帧（``[DONE]`` 不计），等于客户端
    SSE 解析器读到的第几个事件（0 是 role 空帧）；role / finish / usage 帧没有对应的
    客户端流式事件。``at`` 是该帧写之前的绝对 ``time.monotonic()``（与
    ``LoggedRequest.at`` 同原点；``watch`` 的 ``Event.at`` 则是相对 ``env.started_at`` 的）。

    配对口径与账本的生命周期见 ``docs/dev/probe-testing.md``。
    """

    request_index: int
    frame_index: int
    at: float


def _error_response(status: int, message: str, *, code: str) -> web.Response:
    """OpenAI 形态的错误体（``error.message`` 是可读报告文本）。"""
    return web.json_response(
        {"error": {"message": message, "type": "probe_error", "code": code}},
        status=status,
    )


class FakeProvider:
    """按剧本吐确定分片的 OpenAI 兼容假 Provider（同进程 aiohttp）。"""

    def __init__(
        self,
        *,
        host: str = "127.0.0.1",
        port: int = 0,
        path_prefixes: Sequence[str] = (),
    ) -> None:
        self.host = host
        self.path_prefixes = tuple(entry.strip("/") for entry in path_prefixes)
        """附加 provider 的路径前缀（``<prefix>/v1/chat/completions`` 也指向本 app）。

        ``ProbeEnv`` 由 ``extra_providers`` 的 name 生成它们；每个前缀对应配置里
        一个附加 provider 的 base_url，请求留档的 ``path`` 因此能判定归属。
        """
        self.scripts = ScriptRegistry()
        """model → Script 路由表。"""
        self.requests = RequestLog()
        """入站请求留档。"""
        self._frame_emits: list[FrameEmit] = []
        self._emits_by_request: dict[int, list[float]] = {}
        """``frame_emits`` 的按请求索引（``emits_for`` 走它，避免扫全表）。"""
        self._emits_generation: int | None = None
        """写下第一帧时 ``requests`` 的 generation（检测序号复用）。"""
        self._port = port
        self._runner: web.AppRunner | None = None
        self._site: web.SockSite | None = None
        self._socket: socket.socket | None = None

    # ── 生命周期 ────────────────────────────────────────────

    @property
    def started(self) -> bool:
        return self._runner is not None

    @property
    def port(self) -> int:
        """真实监听端口（``port=0`` 时由 OS 分配，start() 后读回）。"""
        return self._port

    @property
    def url(self) -> str:
        return f"http://{self.host}:{self._port}"

    @property
    def base_url(self) -> str:
        """OpenAI 兼容根（provider 配置里的 ``base_url``）。"""
        return f"{self.url}/v1"

    async def start(self) -> FakeProvider:
        """起服务（``port=0`` → OS 分配，读回真实端口）。"""
        if self._runner is not None:
            return self
        app = web.Application()
        app.router.add_post(CHAT_PATH, self._handle_chat_completions)
        for prefix in self.path_prefixes:
            app.router.add_post(f"/{prefix}{CHAT_PATH}", self._handle_chat_completions)
        self._runner = web.AppRunner(app, access_log=None)
        await self._runner.setup()

        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind((self.host, self._port))
        sock.listen(128)
        sock.setblocking(False)
        self._socket = sock
        self._port = int(sock.getsockname()[1])
        self._site = web.SockSite(self._runner, sock)
        await self._site.start()
        return self

    async def stop(self) -> None:
        """停止服务（幂等；未启动时无害）。"""
        if self._site is not None:
            await self._site.stop()
            self._site = None
        if self._runner is not None:
            await self._runner.cleanup()
            self._runner = None
        if self._socket is not None:
            self._socket.close()
            self._socket = None

    async def __aenter__(self) -> FakeProvider:
        return await self.start()

    async def __aexit__(self, *_: Any) -> None:
        await self.stop()

    # ── 剧本注册 ────────────────────────────────────────────

    def register(self, model: str, script: Script) -> Script:
        """注册某 model 的剧本（重复注册抛 ``ValueError``）。"""
        return self.scripts.register(model, script)

    def models(self) -> list[str]:
        return self.scripts.models()

    # ── 发射时刻账本 ────────────────────────────────────────

    @property
    def frame_emits(self) -> tuple[FrameEmit, ...]:
        """流式响应的逐帧发射时刻（只读快照；按**写完**顺序，并发时各请求交错）。

        测量器件而非日志：只增不减、不落盘、无上界，规模由场景的「请求数 × 每轮帧数」
        决定（含按请求索引约百 B / 0.35µs 一条）；要一份干净账本就新建 provider——账本
        与请求留档共享一个号空间，清掉留档就再也归不了因（见 :meth:`emits_for`）。
        每次访问复制一份快照：按轮次读，不要放进帧循环。
        """
        self._require_unreused_indices()
        return tuple(self._frame_emits)

    def emits_for(self, request_index: int) -> list[float]:
        """某次请求的逐帧发射时刻（按 ``frame_index`` 升序，绝对 ``time.monotonic()``）。

        ``request_index`` 传 :class:`FrameEmit` 说的那个全局留档序号。没有帧的请求——
        非流式压缩调用，或在产出任何帧之前就失败（剧本耗尽 5xx）——返回空列表；从未
        留档的下标抛 ``IndexError``：打错下标会得到一张「好看的空表」，那比报错贵得多。
        返回副本，改它不影响账本。
        """
        self._require_unreused_indices()
        recorded = len(self.requests)
        if request_index < 0 or request_index >= recorded:
            raise IndexError(
                f"request #{request_index} out of range "
                f"({recorded} request(s) recorded)"
            )
        return list(self._emits_by_request.get(request_index, ()))

    def _require_unreused_indices(self) -> None:
        """留档被清过就拒绝出账：``clear()`` 让序号从 0 重新发号，账本里的帧随之无法归因。"""
        if (
            self._emits_generation is not None
            and self.requests.generation != self._emits_generation
        ):
            raise RuntimeError(
                "the request log was cleared after frames were recorded: request "
                "indices are reissued from 0, so the ledger's emissions can no longer "
                "be attributed to a request (build a fresh provider instead)"
            )

    # ── HTTP ───────────────────────────────────────────────

    async def _handle_chat_completions(
        self, request: web.Request
    ) -> web.StreamResponse:
        try:
            body = await request.json()
        except (ValueError, UnicodeDecodeError) as exc:
            return _error_response(
                400, f"request body is not valid JSON: {exc}", code="invalid_json"
            )
        if not isinstance(body, dict):
            return _error_response(
                400, "request body must be a JSON object", code="invalid_json"
            )

        raw_model = body.get("model")
        model = raw_model if isinstance(raw_model, str) else ""
        logged = self.requests.record(
            body, model=model, path=request.path, headers=request.headers
        )
        if self._emits_generation is None:
            self._emits_generation = self.requests.generation
        headers = {"x-request-id": f"probe-{logged.index}"}

        try:
            turn, turn_index = self.scripts.consume(model)
        except ScriptError as exc:
            return _error_response(
                500, f"{exc}\n{self.scripts.describe()}", code=exc.code
            )

        completion_id = f"chatcmpl-probe-{logged.index}"
        created = int(logged.at_wall)
        if logged.stream:
            return await self._stream_turn(
                request,
                turn,
                turn_index=turn_index,
                model=model,
                completion_id=completion_id,
                created=created,
                headers=headers,
                request_index=logged.index,
            )
        return web.json_response(
            completion_response(
                turn,
                turn_index=turn_index,
                model=model,
                completion_id=completion_id,
                created=created,
            ),
            headers=headers,
        )

    async def _stream_turn(
        self,
        request: web.Request,
        turn: Turn,
        *,
        turn_index: int,
        model: str,
        completion_id: str,
        created: int,
        headers: dict[str, str],
        request_index: int,
    ) -> web.StreamResponse:
        response = web.StreamResponse(
            status=200,
            headers={
                "Content-Type": SSE_CONTENT_TYPE,
                "Cache-Control": "no-cache",
                **headers,
            },
        )
        await response.prepare(request)
        frames = stream_frames(
            turn,
            turn_index=turn_index,
            model=model,
            completion_id=completion_id,
            created=created,
        )
        try:
            await self._write_frames(response, frames, request_index=request_index)
            await response.write_eof()
        except ConnectionResetError:
            # 客户端中途断开（interrupt 场景的常态）——不是错误。
            return response
        return response

    async def _write_frames(
        self,
        response: web.StreamResponse,
        frames: Sequence[SSEFrame],
        *,
        request_index: int,
    ) -> None:
        """逐帧等待 + 写；每写成一帧登记一次发射时刻（``[DONE]`` 不登记）。"""
        frame_index = 0
        for frame in frames:
            if frame.delay:
                await asyncio.sleep(frame.delay)
            payload = frame.encode()
            at = time.monotonic()
            await response.write(payload)
            if frame.data is None:
                continue
            self._frame_emits.append(
                FrameEmit(request_index=request_index, frame_index=frame_index, at=at)
            )
            self._emits_by_request.setdefault(request_index, []).append(at)
            frame_index += 1


__all__ = ["CHAT_PATH", "FakeProvider", "FrameEmit"]
