# wing/provider/sse.py
"""SSE (Server-Sent Events) 行解析工具。

供 OpenAI 兼容协议和 Anthropic 协议的流式响应解析共用。
处理：data 行拼接、[DONE] 终止、注释行跳过、空行分隔、event 字段。
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncGenerator, AsyncIterator
from dataclasses import dataclass

from wing.common.logger import log


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


# 响应体闲置阈值（秒）：两次读取之间超过它即判定停滞。硬编码——没有按部署
# 差异取值的依据：健康的生成即便很慢也持续吐 token，而"响应头已到、随后
# 一字不发"只可能是坏连接或上游卡死。停滞抛 TimeoutError，交由既有 with_retry
# 重试（不另写重试逻辑）。
STREAM_IDLE_TIMEOUT = 120.0


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
