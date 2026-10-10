"""请求留档 —— 假 Provider 原样保留每次入站请求（design D3）。

留档是"上下文事实"的唯一来源：断言不看网关内部，只看"网关实际发出去的
LLM 请求长什么样"。检索维度：model 名 + 序号（全局序号与同 model 内序号）。

``LoggedRequest`` 冻结原始 body（不做任何规范化）；需要规范化视图时走
``LoggedRequest.context()`` → ``ContextView``。**请求头**（``headers``）也留档：
「网关实际用哪份凭据调用上游」是密文语义（§7.5：`null` = 保留现值）唯一可观测的
证据面——body 里没有凭据，只有请求头的 ``Authorization`` 有。
"""

from __future__ import annotations

import time
from collections.abc import Iterator, Mapping
from dataclasses import dataclass, field
from typing import Any

from wing_probe.provider.context import ContextView


def normalize_headers(headers: Mapping[str, str] | None) -> dict[str, str]:
    """入站请求头 → 小写键字典（HTTP 头不区分大小写；断言侧不必再折叠）。

    值原样保留（含 ``Bearer <key>``）；同名头只留最后一个（重复头在语义上等价于
    逗号连接，这里不需要那个精度）。
    """
    if not headers:
        return {}
    return {str(key).lower(): str(value) for key, value in headers.items()}


@dataclass(frozen=True)
class LoggedRequest:
    """一次入站 ``chat/completions`` 请求的留档。"""

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
    path: str = ""
    """入站请求的原始路径（``request.path``）。

    主 provider 是 ``/v1/chat/completions``；附加 provider（跨 provider 场景）
    是 ``/<name>/v1/chat/completions``——「这次调用打到哪个 provider 的端点」
    因此是可断言事实。剧本仍按**调用名**路由（同名调用名共用一条剧本队列）。
    """
    headers: Mapping[str, str] = field(default_factory=dict)
    """入站请求头（**小写键**；见 :func:`normalize_headers`）。

    密文断言的证据面：``headers["authorization"] == f"Bearer {key}"`` 是
    「这次调用用的是哪把钥匙」的唯一可观测事实（假 Provider 不校验它）。
    刻意**不进**失败现场转储（``probe.dump``）：artifacts 不该出现凭据。
    """

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
        path: str = "",
        headers: Mapping[str, str] | None = None,
        at: float | None = None,
        at_wall: float | None = None,
    ) -> LoggedRequest:
        """记录一次请求（原样保留 body 与请求头），返回留档条目。"""
        entry = LoggedRequest(
            index=len(self._requests),
            model=model,
            body=body,
            at=time.monotonic() if at is None else at,
            at_wall=time.time() if at_wall is None else at_wall,
            model_index=len(self.by_model(model)),
            path=path,
            headers=normalize_headers(headers),
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
