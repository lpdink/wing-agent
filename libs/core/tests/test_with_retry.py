"""with_retry 装饰器的取消语义测试。

事件系统依赖：流式生成期间用户打断（task.cancel → CancelledError）必须
直通到消费者（react_loop._call_llm 的 except CancelledError 补提交路径），
绝不能被重试装饰器捕获为可重试错误——否则半截内容永远无法 snapshot。

Python 3.8+ CancelledError 继承 BaseException（非 Exception），
except Exception 天然不捕获——本文件钉死该契约，防止未来重构回退。
"""

from __future__ import annotations

import asyncio

import pytest

from wing.common.with_retry import with_retry


class Flaky:
    """带 _config 的宿主实例（模拟 provider）。"""

    def __init__(self) -> None:
        self._config = type("C", (), {"max_retries": 2, "max_retry_delay": 0.01})()
        self.attempts = 0


class TestCancelledErrorPassthrough:
    @pytest.mark.asyncio
    async def test_coroutine_cancel_propagates(self):
        host = Flaky()

        @with_retry(max_retries=5)
        async def slow(provider, fail_first: bool = False):
            # provider 位承载 mock host——with_retry 经 args[0] 读 _config
            assert provider is host
            host.attempts += 1
            if fail_first and host.attempts == 1:
                raise RuntimeError("retryable")
            await asyncio.sleep(30)

        task = asyncio.create_task(slow(host))
        await asyncio.sleep(0)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        # 未被重试吞掉：只有一次真实进入
        assert host.attempts == 1

    @pytest.mark.asyncio
    async def test_coroutine_cancel_during_retry_sleep_propagates(self):
        """重试退避等待中被取消：同样直通，不进入下一轮尝试。"""
        host = Flaky()

        @with_retry(max_retries=5, base_delay=10.0)
        async def failing(provider):
            assert provider is host  # provider 位承载 mock host
            host.attempts += 1
            raise RuntimeError("retryable")

        task = asyncio.create_task(failing(host))
        await asyncio.sleep(0)  # 第一次失败，进入退避
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert host.attempts == 1

    @pytest.mark.asyncio
    async def test_generator_cancel_mid_stream_propagates(self):
        """async generator 消费中途被取消：CancelledError 直通消费者。"""
        host = Flaky()

        @with_retry(max_retries=5)
        async def stream(provider):
            assert provider is host  # provider 位承载 mock host
            host.attempts += 1
            yield "chunk-1"
            await asyncio.sleep(30)
            yield "chunk-2"  # pragma: no cover

        consumed: list[str] = []
        task = asyncio.create_task(_consume(stream(host), consumed))
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert consumed == ["chunk-1"]
        assert host.attempts == 1

    @pytest.mark.asyncio
    async def test_generator_retryable_error_still_retries(self):
        """对照：普通 Exception 仍被重试（确保 passthrough 不是吞掉一切）。"""
        host = Flaky()

        @with_retry(max_retries=3, base_delay=0.001)
        async def flaky_stream(provider):
            assert provider is host  # provider 位承载 mock host
            host.attempts += 1
            if host.attempts < 3:
                raise RuntimeError("retryable")
            yield "ok"

        items = [item async for item in flaky_stream(host)]
        assert items == ["ok"]
        assert host.attempts == 3


async def _consume(gen, sink: list) -> None:
    async for item in gen:
        sink.append(item)
