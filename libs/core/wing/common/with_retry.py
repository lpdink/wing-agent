import asyncio
import inspect
from collections.abc import AsyncIterator, Callable
from functools import wraps
from typing import ParamSpec, TypeVar

from .logger import log

P = ParamSpec("P")
T = TypeVar("T")


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
                            f"call {fn_name} failed (attempt {attempt + 1}), retrying..."
                        )
                        delay = base_delay * (2**attempt)
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
                        f"call {fn_name} failed (attempt {attempt + 1}), retrying..."
                    )
                    delay = base_delay * (2**attempt)
                    await asyncio.sleep(delay)

        return async_gen_wrapper

    return decorator
