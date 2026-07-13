import asyncio
import inspect
from collections.abc import AsyncIterator, Callable
from functools import wraps
from typing import ParamSpec, TypeVar

from .logger import log

P = ParamSpec("P")
T = TypeVar("T")


def _emit_retry_event(
    attempt: int, max_retries: int, fn_name: str, exc: Exception, delay: float
) -> None:
    """通过 EventBus 向前端发送重试通知。懒加载避免循环导入。"""
    try:
        from wing.event import ErrorEvent
        from wing.event_bus import event_bus

        error_detail = f"{type(exc).__name__}: {exc}"
        if exc.__cause__:
            error_detail += (
                f" (caused by {type(exc.__cause__).__name__}: {exc.__cause__})"
            )
        event_bus.emit(
            ErrorEvent(
                message=f"{fn_name} 调用失败 ({attempt + 1}/{max_retries}): {error_detail} {delay:.0f}s 后重试"
            )
        )
    except Exception:
        pass  # 事件通知失败不应影响重试逻辑


def with_retry(max_retries: int, base_delay: float = 3.0) -> Callable:
    """指数退避重试装饰器 - 支持普通 async 函数和 async generators"""

    def decorator(func: Callable) -> Callable:
        fn_name = getattr(func, "__name__", repr(func))
        if inspect.iscoroutinefunction(func):

            @wraps(func)
            async def wrapper(*args: P.args, **kwargs: P.kwargs) -> object:
                last_exc: Exception | None = None
                for attempt in range(max_retries + 1):
                    try:
                        return await func(*args, **kwargs)
                    except Exception as e:
                        last_exc = e
                        if attempt == max_retries:
                            raise last_exc
                        log.error(
                            f"call {fn_name} failed (attempt {attempt + 1}): {type(e).__name__}: {e}, retrying..."
                        )
                        delay = base_delay * (2**attempt)
                        _emit_retry_event(attempt, max_retries, fn_name, e, delay)
                        await asyncio.sleep(delay)
                raise last_exc  # ty: ignore # unreachable

            return wrapper

        # async generator
        @wraps(func)
        async def async_gen_wrapper(
            *args: P.args, **kwargs: P.kwargs
        ) -> AsyncIterator[object]:
            last_exc: Exception | None = None
            for attempt in range(max_retries + 1):
                try:
                    async for item in func(*args, **kwargs):
                        yield item
                    return  # success, exit generator
                except Exception as e:
                    last_exc = e
                    if attempt == max_retries:
                        raise last_exc
                    log.error(
                        f"call {fn_name} failed (attempt {attempt + 1}): {type(e).__name__}: {e}, retrying..."
                    )
                    delay = base_delay * (2**attempt)
                    _emit_retry_event(attempt, max_retries, fn_name, e, delay)
                    await asyncio.sleep(delay)

        return async_gen_wrapper

    return decorator
