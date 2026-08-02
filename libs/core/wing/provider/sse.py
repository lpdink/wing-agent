# wing/provider/sse.py
"""SSE (Server-Sent Events) 行解析工具。

供 OpenAI 兼容协议和 Anthropic 协议的流式响应解析共用。
处理：data 行拼接、[DONE] 终止、注释行跳过、空行分隔、event 字段。
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import AsyncIterator


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


def parse_json_event(event: SSEEvent) -> dict | None:
    """将 SSE 事件的 data 解析为 JSON dict。解析失败返回 None。"""
    try:
        return json.loads(event.data)
    except (json.JSONDecodeError, ValueError):
        return None
