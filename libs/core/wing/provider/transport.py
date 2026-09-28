# wing/provider/transport.py
"""provider 层共享的传输管道与错误面（两个协议同构）。

- httpx client 构造（统一超时口径）；
- SSE 行解析与响应体闲置超时（OpenAI / Anthropic 流式共用）；
- HTTP / 流错误类型与 `raise_with_body`（错误 body 进异常现场）。

只做「传输」——协议差异（请求体与流事件语义）留在两个 provider 实现。
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncGenerator, AsyncIterator
from dataclasses import dataclass

import httpx

from wing.common.logger import log

# ── httpx 构造 ──────────────────────────────────────────────────

# httpx 层超时是底层兜底——必须 > 应用层 timeout_total（默认 600s），
# 否则非流式调用（如压缩 LLM decode）会在 httpx 层被杀，先于 asyncio.wait_for
# 的 timeout_total 触发。设 1200s（2x timeout_total）确保不干扰应用层控制。
# 实际生效的超时由调用侧 asyncio.wait_for
# （timeout_first_chunk / timeout_total）控制。
_DEFAULT_TIMEOUT = 1200.0
_CONNECT_TIMEOUT = 5.0


def make_http_timeout() -> httpx.Timeout:
    """provider httpx client 的统一超时构造。"""
    return httpx.Timeout(_DEFAULT_TIMEOUT, connect=_CONNECT_TIMEOUT)


# ── SSE 行解析 ──────────────────────────────────────────────────

# 响应体闲置阈值（秒）：两次读取之间超过它即判定停滞。硬编码——没有按部署
# 差异取值的依据：健康的生成即便很慢也持续吐 token，而"响应头已到、随后
# 一字不发"只可能是坏连接或上游卡死。停滞抛 TimeoutError，交由既有 with_retry
# 重试（不另写重试逻辑）。
STREAM_IDLE_TIMEOUT = 120.0


@dataclass
class SSEEvent:
    """一个完整的 SSE 事件。"""

    event: str = ""
    data: str = ""


class SSEParser:
    """增量式 SSE 解析器。

    逐行喂入（feed），在遇到空行时产出完整事件。
    支持多行 data 拼接（用 \\n 连接）。
    """

    def __init__(self) -> None:
        self._event_type: str = ""
        self._data_lines: list[str] = []

    def feed(self, line: str) -> SSEEvent | None:
        """喂入一行（不含尾部换行符）。返回完整事件或 None。"""
        # 注释行（以 : 开头）——keep-alive ping 等
        if line.startswith(":"):
            return None

        # 空行 = 事件边界
        if not line:
            if self._data_lines:
                event = SSEEvent(
                    event=self._event_type,
                    data="\n".join(self._data_lines),
                )
                self._event_type = ""
                self._data_lines = []
                return event
            # 无数据的空行，重置状态
            self._event_type = ""
            return None

        # 解析 field: value
        if ":" in line:
            field_name, _, value = line.partition(":")
            # SSE 规范：冒号后有一个可选空格
            if value.startswith(" "):
                value = value[1:]
        else:
            field_name = line
            value = ""

        if field_name == "data":
            self._data_lines.append(value)
        elif field_name == "event":
            self._event_type = value
        # 其他字段（id, retry）忽略

        return None


async def parse_sse_stream(
    lines: AsyncIterator[str],
) -> AsyncIterator[SSEEvent]:
    """将异步行迭代器转换为 SSE 事件流。

    跳过 data 为 "[DONE]" 的终止事件。
    """
    parser = SSEParser()
    async for line in lines:
        event = parser.feed(line)
        if event is not None:
            if event.data == "[DONE]":
                return
            yield event


async def lines_with_idle_timeout(
    lines: AsyncIterator[str],
    *,
    timeout: float = STREAM_IDLE_TIMEOUT,
    context: str = "LLM stream",
) -> AsyncGenerator[str]:
    """逐行透传 `lines`，但两次读取之间的间隔不得超过 `timeout` 秒。

    闲置超时抛 `TimeoutError`（内建类型，与 timeout_total / timeout_first_chunk
    的失败同类）；正常结束（含 `StopAsyncIteration`）不抛。调用方负责在
    finally 里 `aclose()` 响应体——超时后底层流可能仍停在读中。

    `context` 只用于日志与异常文案（响应头已收到，必须说清是谁的响应体停滞）。
    """
    while True:
        try:
            line = await asyncio.wait_for(anext(lines), timeout=timeout)
        except StopAsyncIteration:
            return
        except TimeoutError:
            # 响应头超时（timeout_first_chunk）走不到这里：本函数只包响应体读取。
            log.error(
                f"{context}: no data for {timeout:.0f}s after response header — "
                f"treating the stream as stalled"
            )
            raise TimeoutError(
                f"{context} stalled: no data for {timeout:.0f}s after response header"
            ) from None
        yield line


def parse_json_event(event: SSEEvent) -> dict | None:
    """将 SSE 事件的 data 解析为 JSON dict。解析失败返回 None。"""
    try:
        return json.loads(event.data)
    except (json.JSONDecodeError, ValueError):
        return None


# ── 错误面 ──────────────────────────────────────────────────────

_BODY_SNIPPET_LIMIT = 1000


class ProviderHTTPError(Exception):
    """模型 API 返回 4xx/5xx（携带响应 body）。"""

    def __init__(self, status_code: int, url: str, body: str) -> None:
        self.status_code = status_code
        self.url = url
        self.body = body
        snippet = body[:_BODY_SNIPPET_LIMIT] if body else "<empty body>"
        super().__init__(f"LLM API error {status_code} for url {url}: {snippet}")


class ProviderStreamError(Exception):
    """流中 error 事件（overloaded / rate limit / 中途终止等）。

    抛出后 with_retry 触发重试；重试耗尽经 loop 错误路径上报——截断轮次
    绝不作为成功 turn 提交。
    """

    def __init__(self, data: dict) -> None:
        self.data = data
        err = data.get("error") or {}
        super().__init__(f"LLM stream error: {err.get('message') or data}")


async def raise_with_body(resp: httpx.Response) -> None:
    """4xx/5xx 时先读 body 再抛 ProviderHTTPError；2xx/3xx 直通。"""
    if not resp.is_error:
        return
    await resp.aread()
    log.error(f"LLM API error {resp.status_code} url={resp.url} body={resp.text}")
    raise ProviderHTTPError(resp.status_code, str(resp.url), resp.text)
