"""tool result 截断的唯一算法。

两处调用点的阈值来源不同且刻意保持独立：

- ``agent/tool_executor`` 走配置 ``tool_result_truncate``（默认 100k / 200 / 200）；
- ``libs/wing_hooks`` 的 ``truncate_tool_result`` 是独立 hook，硬编码 50k / 100 / 100，
  不读配置——它可以在网关之外单独装载。

但"头 + 标记 + 尾"的拼装只有这一份：标记文案里的长度与落盘提示是模型可见输出，
散成两份必然漂移。
"""

from __future__ import annotations


def head_tail_marker(result: str, *, keep_head: int, keep_tail: int, note: str) -> str:
    """截断成 ``head + marker + tail``。

    ``note`` 是标记括号里的最后一段（落盘路径提示或失败说明）；``keep_head`` /
    ``keep_tail`` 为 0 时对应一侧为空。返回的字节由入参唯一决定。
    """
    head = result[:keep_head] if keep_head > 0 else ""
    tail = result[-keep_tail:] if keep_tail > 0 else ""
    marker = f"... [truncated, original length: {len(result)} chars, {note}]"
    return f"{head}\n{marker}\n{tail}"
