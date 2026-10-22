# wing/agent/diagnostics.py
"""worker 挂起点的诊断投影（asyncio 协程栈内省）。

与 agent / 会话语义无关的一段自洽逻辑：``Task.get_stack()`` 只给顶层帧，这里沿
``cr_await`` / ``ag_await`` / ``gi_yieldfrom`` 下钻到真正的暂停点。任何异常都退回
``get_stack()`` 或占位符——**诊断不得外溢**，它只在"worker 不响应取消"的错误路径上
被调用。
"""

from __future__ import annotations

import asyncio
import gc
import types
from collections import deque
from pathlib import Path
from typing import Any


#: 无帧中转对象：唯一 awaitable 引用只挂在 GC referents 上（见 `_descend`）。
_REFERENT_WRAPPERS = frozenset(
    {"async_generator_asend", "async_generator_athrow", "FutureIter"}
)

#: GC referents 兜底只认这些类型，避免误抓无关引用。
_REFERENT_AWAITABLES: tuple[type, ...] = (
    asyncio.Future,
    types.CoroutineType,
    types.AsyncGeneratorType,
    types.GeneratorType,
)


def _worker_frames(worker: asyncio.Task[Any], *, limit: int = 8) -> str:
    """worker 挂起点的浅栈（最内层在左，`<-` 连接）。

    `Task.get_stack()` 在 3.12 只给顶层帧；这里沿 `cr_await` / `ag_await` /
    `gi_yieldfrom` 一路走到真正的暂停点，并穿透三类无帧中转对象：`await
    <Task>`（FutureIter）与 `async for`（asend）经 GC referents 找唯一
    awaitable，gather 的 `_GatheringFuture` 经 `_children` 取单一 child
    （多 child 只标注不猜）。超长链截断保留最内层。任何异常都退回
    `get_stack()` 或占位符——诊断不得外溢。
    """

    def _location(obj: Any) -> str:
        return (
            f"{Path(obj.f_code.co_filename).name}:{obj.f_lineno}:{obj.f_code.co_name}"
        )

    def _frame_of(obj: Any) -> Any:
        return (
            getattr(obj, "cr_frame", None)
            or getattr(obj, "ag_frame", None)
            or getattr(obj, "gi_frame", None)
        )

    def _descend(current: Any) -> tuple[Any, str | None]:
        """下钻一层：返回 (下一跳, 标记)；标记用于无法下钻的分叉点。"""
        nxt = (
            getattr(current, "cr_await", None)
            or getattr(current, "ag_await", None)
            or getattr(current, "gi_yieldfrom", None)
        )
        if nxt is not None:
            return nxt, None
        if isinstance(current, asyncio.Task):
            return current.get_coro(), None
        children = getattr(current, "_children", None)  # asyncio.gather 的 future
        if children is not None:
            kids = list(children)
            if len(kids) == 1:
                return kids[0], None
            return None, f"<gather ×{len(kids)}>"
        if type(current).__name__ in _REFERENT_WRAPPERS:
            candidates = [
                ref
                for ref in gc.get_referents(current)
                if isinstance(ref, _REFERENT_AWAITABLES)
            ]
            return (candidates[0], None) if len(candidates) == 1 else (None, None)
        return None, None

    try:
        # deque(maxlen)：超长链时保留**最内层**（真正的暂停点），而不是外层；
        # 被截掉的外层在输出右端以 `…` 标注（读日志的人要能分辨"停在那"与
        # "被截断"）。
        steps: deque[Any] = deque(maxlen=limit)
        visited = 0
        current: Any = worker.get_coro()
        seen: set[int] = set()
        while current is not None and id(current) not in seen:
            seen.add(id(current))
            frame = _frame_of(current)
            if frame is not None:
                steps.append(frame)
                visited += 1
            current, marker = _descend(current)
            if marker is not None:
                steps.append(marker)
                visited += 1
        if not steps:
            steps.extend(worker.get_stack()[-4:])
        text = " <- ".join(
            step if isinstance(step, str) else _location(step)
            for step in reversed(steps)
        )
        if visited > limit:
            text = f"{text} <- …"
        return text or "<none>"
    except Exception:  # noqa: BLE001 - 诊断不得外溢
        return "<unavailable>"
