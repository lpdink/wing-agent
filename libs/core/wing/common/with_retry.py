import asyncio
import inspect
from collections.abc import AsyncIterator, Callable
from functools import wraps
from typing import ParamSpec, TypeVar

from .logger import log

P = ParamSpec("P")
T = TypeVar("T")

# 默认最大重试次数（config 未提供时兜底）。
DEFAULT_MAX_RETRIES = 10
# 默认最大重试间隔（秒，3 分钟）。指数退避被 clamp 到该上限。
DEFAULT_MAX_DELAY = 180.0


def _emit_retry_event(
    attempt: int, max_retries: int, fn_name: str, exc: Exception, delay: float
) -> None:
    """通过 EventBus 向前端发送重试通知。懒加载避免循环导入。"""
    try:
        from wing.event import ErrorEvent, EventTarget
        from wing.event_bus import event_bus

        from .utils import format_exception_chain

        error_detail = format_exception_chain(exc)
        event_bus.emit(
            ErrorEvent(
                message=f"{fn_name} 调用失败 ({attempt + 1}/{max_retries}): {error_detail}, {delay:.0f}s 后重试",
                target=EventTarget(scope="session"),
            )
        )
    except Exception as e:
        log.debug(f"Failed to emit retry event: {e}")


def _resolve_retry_params(
    self: object, max_retries: int | None, max_delay: float | None
) -> tuple[int, float]:
    """从调用方实例的 config 解析重试参数（未显式传入时）。

    优先读取 ``self._config.max_retries`` / ``self._config.max_retry_delay``，
    缺失时回落到默认值。
    """
    cfg = getattr(self, "_config", None)
    if cfg is not None:
        if max_retries is None:
            max_retries = getattr(cfg, "max_retries", DEFAULT_MAX_RETRIES)
        if max_delay is None:
            max_delay = getattr(cfg, "max_retry_delay", DEFAULT_MAX_DELAY)
    return (
        max_retries if max_retries is not None else DEFAULT_MAX_RETRIES,
        max_delay if max_delay is not None else DEFAULT_MAX_DELAY,
    )


def with_retry(
    max_retries: int | None = None,
    base_delay: float = 3.0,
    max_delay: float | None = None,
) -> Callable:
    """指数退避重试装饰器 - 支持普通 async 函数和 async generators。

    max_retries: 最大重试次数。为 None 时从被装饰实例的 config 读取
        (``self._config.max_retries``)，默认 10。
    base_delay: 基础延迟（秒）。
    max_delay: 最大延迟上限（秒）。为 None 时从 config 读取
        (``self._config.max_retry_delay``)，默认 180 秒（3 分钟）。
        延迟计算为 ``delay = min(base_delay * 2**attempt, max_delay)``，
        避免退避无限增长。
    """

    def decorator(func: Callable) -> Callable:
        fn_name = getattr(func, "__name__", repr(func))
        if inspect.iscoroutinefunction(func):

            @wraps(func)
            async def wrapper(*args: P.args, **kwargs: P.kwargs) -> object:
                resolved_retries, resolved_max_delay = _resolve_retry_params(
                    args[0] if args else None, max_retries, max_delay
                )
                last_exc: Exception | None = None
                for attempt in range(resolved_retries + 1):
                    try:
                        return await func(*args, **kwargs)
                    except Exception as e:
                        last_exc = e
                        if attempt == resolved_retries:
                            raise last_exc
                        log.error(
                            f"call {fn_name} failed (attempt {attempt + 1}): {type(e).__name__}: {e}, retrying..."
                        )
                        delay = min(base_delay * (2**attempt), resolved_max_delay)
                        _emit_retry_event(attempt, resolved_retries, fn_name, e, delay)
                        await asyncio.sleep(delay)
                raise last_exc  # ty: ignore # unreachable

            return wrapper

        # async generator
        @wraps(func)
        async def async_gen_wrapper(
            *args: P.args, **kwargs: P.kwargs
        ) -> AsyncIterator[object]:
            resolved_retries, resolved_max_delay = _resolve_retry_params(
                args[0] if args else None, max_retries, max_delay
            )
            last_exc: Exception | None = None
            for attempt in range(resolved_retries + 1):
                try:
                    async for item in func(*args, **kwargs):
                        yield item
                    return  # success, exit generator
                except Exception as e:
                    last_exc = e
                    if attempt == resolved_retries:
                        raise last_exc
                    log.error(
                        f"call {fn_name} failed (attempt {attempt + 1}): {type(e).__name__}: {e}, retrying..."
                    )
                    delay = min(base_delay * (2**attempt), resolved_max_delay)
                    _emit_retry_event(attempt, resolved_retries, fn_name, e, delay)
                    await asyncio.sleep(delay)

        return async_gen_wrapper

    return decorator
