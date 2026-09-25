"""HookRegistry — hook 管道系统。

核心设计：
  1. hooks.on(point_name) 注册 handler，point_name 是字符串，开放扩展
  2. handler 按注册顺序串联执行
  3. handler 签名：(value, **context) → value | None
     - value 是被修改的主值，管道串联传递
     - context 是常量上下文（如 tool_name），整个管道中不变
  4. handler 返回非 None → 替换 value
  5. handler 返回 None → 保留当前 value
  6. handler 异常 → 跳过，保留当前 value，log warning
  7. 同一函数注册到同一 point 只生效一次
  8. invoke_async 支持 sync + async handler 混合执行

Hook ≠ Event。Hook 是拦截/修改管道，Event 是通知广播。
"""

from __future__ import annotations

import inspect
from collections import defaultdict
from collections.abc import Callable
from typing import Any

from wing.common.logger import log


class HookRegistry:
    """hook 管道注册中心。

    用法：
        hooks = HookRegistry()
        hooks.on("before_user_message")(my_handler)

    调用：
        result = hooks.invoke("before_user_message", value, **context)
        result = await hooks.invoke_async("before_user_message", value, **context)

    handler 签名：
        def handler(value, **context) -> value | None
        async def handler(value, **context) -> value | None

    value 是管道串联的主值，context 是常量上下文。
    """

    def __init__(self) -> None:
        self._handlers: dict[str, list[Callable]] = defaultdict(list)

    def on(self, point: str) -> Callable[[Callable], Callable]:
        """注册 handler 到指定 hook point。

        Returns decorator that registers the handler and returns it unchanged.
        同一函数注册到同一 point 只生效一次（静默忽略重复）。
        """

        def decorator(fn: Callable) -> Callable:
            handlers = self._handlers[point]
            if fn not in handlers:
                handlers.append(fn)
            else:
                name = getattr(fn, "__name__", repr(fn))
                log.warning(
                    f"Hook handler {name} already registered "
                    f"for point '{point}', skipping"
                )
            return fn

        return decorator

    def handlers(self, point: str) -> list[Callable]:
        """查询某个 point 的 handler 列表（只读快照）"""
        return list(self._handlers.get(point, []))

    def clear(self) -> None:
        """清空所有已注册的 handler，为 reload 做准备。

        注意：会清除所有 handler，不区分来源。
        当前所有 hooks 均来自 config 路径加载，故 reload 安全。
        若未来 core 注册内置 hooks，需改为按 source 选择性清除。
        """
        self._handlers.clear()

    def invoke(self, point: str, value: Any, **context: Any) -> Any:
        """同步执行 hook 管道。

        依次执行所有 handler，串联 value：
        - handler 返回非 None → 替换当前 value
        - handler 返回 None → 保留当前 value
        - handler 异常 → 跳过，保留当前 value，log warning
        context 在整个管道中不变。
        """
        for fn in self._handlers.get(point, []):
            if inspect.iscoroutinefunction(fn):
                log.warning(
                    f"Hook handler {getattr(fn, '__name__', repr(fn))} "
                    f"for point '{point}' is async, "
                    f"but invoke() is synchronous. Skipping. Use invoke_async() instead."
                )
                continue
            try:
                result = fn(value, **context)
                if result is not None:
                    value = result
            except Exception as e:
                log.warning(
                    f"Hook handler {getattr(fn, '__name__', repr(fn))} "
                    f"for point '{point}' "
                    f"raised {e}, skipping"
                )
        return value

    async def invoke_async(self, point: str, value: Any, **context: Any) -> Any:
        """异步执行 hook 管道。

        支持 sync + async handler 混合执行：
        - async handler → await fn(value, **context)
        - sync handler → fn(value, **context)
        - handler 返回非 None → 替换当前 value
        - handler 返回 None → 保留当前 value
        - handler 异常 → 跳过，保留当前 value，log warning
        context 在整个管道中不变。
        """
        for fn in self._handlers.get(point, []):
            try:
                if inspect.iscoroutinefunction(fn):
                    result = await fn(value, **context)
                else:
                    result = fn(value, **context)
                if result is not None:
                    value = result
            except Exception as e:
                log.warning(
                    f"Hook handler {getattr(fn, '__name__', repr(fn))} "
                    f"for point '{point}' "
                    f"raised {e}, skipping"
                )
        return value


# 全局单例
hooks = HookRegistry()
