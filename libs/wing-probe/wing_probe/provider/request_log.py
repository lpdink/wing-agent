"""请求留档 —— 假 Provider 原样保留每次入站请求（design D3）。

留档是"上下文事实"的唯一来源：断言不看网关内部，只看"网关实际发出去的
LLM 请求长什么样"。检索维度：model 名 + 序号（全局序号与同 model 内序号）。

``LoggedRequest`` 冻结原始 body（不做任何规范化）；需要规范化视图时走
``LoggedRequest.context()`` → ``ContextView``。
"""

from __future__ import annotations

import time
from collections.abc import Iterator
from dataclasses import dataclass, field
from typing import Any

from wing_probe.provider.context import ContextView


@dataclass(frozen=True)
class LoggedRequest:
    """一次入站 ``/v1/chat/completions`` 请求的留档。"""

    index: int
    """全局序号（0-based，按到达顺序）。"""
    model: str
    body: dict[str, Any]
    """原始请求体（原样保留，未做任何规范化）。"""
    at: float
    """相对单调时钟（``time.monotonic()``）。"""
    at_wall: float
    """墙钟时间（``time.time()``，报告可读性用）。"""
    model_index: int = 0
    """同 model 内的序号（0-based）。"""

    @property
    def stream(self) -> bool:
        return bool(self.body.get("stream"))

    @property
    def message_count(self) -> int:
        messages = self.body.get("messages")
        return len(messages) if isinstance(messages, list) else 0

    def context(self) -> ContextView:
        """本次请求的上下文规范化视图（带可定位的来源标签）。"""
        return ContextView(self.body, source=self.label())

    def label(self) -> str:
        return (
            f"request #{self.index} (model={self.model!r}, model #{self.model_index})"
        )

    def describe(self) -> str:
        return (
            f"{self.label()} stream={self.stream} "
            f"messages={self.message_count} at={self.at:.3f}"
        )


@dataclass
class RequestLog:
    """请求留档表（按到达顺序）。"""

    _requests: list[LoggedRequest] = field(default_factory=list)

    def record(
        self,
        body: dict[str, Any],
        *,
        model: str,
        at: float | None = None,
        at_wall: float | None = None,
    ) -> LoggedRequest:
        """记录一次请求（原样保留 body），返回留档条目。"""
        entry = LoggedRequest(
            index=len(self._requests),
            model=model,
            body=body,
            at=time.monotonic() if at is None else at,
            at_wall=time.time() if at_wall is None else at_wall,
            model_index=len(self.by_model(model)),
        )
        self._requests.append(entry)
        return entry

    def all(self) -> list[LoggedRequest]:
        return list(self._requests)

    def by_model(self, model: str) -> list[LoggedRequest]:
        return [entry for entry in self._requests if entry.model == model]

    def get(self, model: str | None = None, index: int = 0) -> LoggedRequest:
        """按 model（``None`` = 全部）与序号取留档；越界抛 ``IndexError``。"""
        entries = self.all() if model is None else self.by_model(model)
        scope = "any model" if model is None else f"model {model!r}"
        if not entries:
            raise IndexError(
                f"no requests recorded for {scope} "
                f"({len(self._requests)} recorded in total)"
            )
        if index < 0 or index >= len(entries):
            raise IndexError(
                f"request #{index} out of range for {scope} "
                f"({len(entries)} recorded for it)"
            )
        return entries[index]

    def last(self, model: str | None = None) -> LoggedRequest:
        entries = self.all() if model is None else self.by_model(model)
        if not entries:
            scope = "any model" if model is None else f"model {model!r}"
            raise IndexError(f"no requests recorded for {scope}")
        return entries[-1]

    def count(self, model: str | None = None) -> int:
        if model is None:
            return len(self._requests)
        return len(self.by_model(model))

    def clear(self) -> None:
        self._requests.clear()

    def summary(self) -> str:
        """按 model 分组的计数概览（失败报告 / 调试用）。"""
        if not self._requests:
            return "0 requests"
        counts: dict[str, int] = {}
        for entry in self._requests:
            counts[entry.model] = counts.get(entry.model, 0) + 1
        groups = ", ".join(f"{name}×{count}" for name, count in sorted(counts.items()))
        return f"{len(self._requests)} request(s): {groups}"

    def __len__(self) -> int:
        return len(self._requests)

    def __iter__(self) -> Iterator[LoggedRequest]:
        return iter(list(self._requests))


__all__ = ["LoggedRequest", "RequestLog"]
