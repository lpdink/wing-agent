"""with_retry 装饰器的取消语义测试。

事件系统依赖：流式生成期间用户打断（task.cancel → CancelledError）必须
直通到消费者（react_loop._call_llm 的 except CancelledError 补提交路径），
绝不能被重试装饰器捕获为可重试错误——否则半截内容永远无法 snapshot。

Python 3.8+ CancelledError 继承 BaseException（非 Exception），
except Exception 天然不捕获——本文件钉死该契约，防止未来重构回退。
"""

from __future__ import annotations

import asyncio
from unittest import mock

import pytest

from wing.common.with_retry import (
    DEFAULT_MAX_DELAY,
    DEFAULT_MAX_RETRIES,
    with_retry,
)


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


# ============================================================
# 恢复 #62 的可配置重试测试（事件系统重写本文件时被误删——追加而非替换，
# 与上方 cancel-passthrough 测试共存）。
# ============================================================


class FakeConfig:
    """模拟 ProviderConfig，携带可配置的重试参数。"""

    def __init__(self, max_retries=None, max_retry_delay=None):
        self.max_retries = max_retries
        self.max_retry_delay = max_retry_delay


class FakeProvider:
    """模拟 LLM provider：携带 self._config 供装饰器解析。"""

    def __init__(self, config=None):
        self._config = config or FakeConfig()


async def _noop_sleep(delay):
    """无操作 sleep，避免测试真实等待退避时间。"""
    return None


class _AsyncCallable:
    """可被装饰的 async 方法（绑定到 provider 实例）。"""

    def __init__(self, provider, fail_times):
        self._provider = provider
        # 装饰器通过 self._config 读取重试参数
        self._config = provider._config
        self._fail_times = fail_times
        self.calls = 0

    @with_retry()
    async def run(self):
        self.calls += 1
        if self.calls <= self._fail_times:
            raise RuntimeError("boom")
        return "ok"


class TestRetryDefaults:
    """默认最大重试次数为 10。"""

    def test_default_max_retries_constant(self):
        assert DEFAULT_MAX_RETRIES == 10

    def test_default_max_delay_constant(self):
        assert DEFAULT_MAX_DELAY == 180.0

    def test_default_retries_on_config(self):
        """config 未配置重试项时，默认最大重试次数为 10。"""
        provider = FakeProvider(config=FakeConfig())
        obj = _AsyncCallable(provider, fail_times=10)
        # 失败 10 次后第 11 次成功（总尝试 = max_retries + 1）
        with mock.patch("wing.common.with_retry.asyncio.sleep", new=_noop_sleep):
            result = asyncio.run(obj.run())
        assert result == "ok"
        assert obj.calls == DEFAULT_MAX_RETRIES + 1

    def test_exceed_max_retries_raises(self):
        """超过最大重试次数后抛出异常。"""
        provider = FakeProvider(config=FakeConfig())
        obj = _AsyncCallable(provider, fail_times=11)
        with mock.patch("wing.common.with_retry.asyncio.sleep", new=_noop_sleep):
            with pytest.raises(RuntimeError, match="boom"):
                asyncio.run(obj.run())
        assert obj.calls == DEFAULT_MAX_RETRIES + 1


class TestRetryDelayCap:
    """指数退避被 clamp 到 max_delay（180s）。"""

    def test_delay_clamped_to_max_delay(self):
        """延迟不超过 max_delay，且超过上限后保持在上限。"""
        provider = FakeProvider(
            config=FakeConfig(max_retries=10, max_retry_delay=180.0)
        )
        obj = _AsyncCallable(provider, fail_times=10)

        sleeps = []

        async def fake_sleep(delay):
            sleeps.append(delay)

        with mock.patch("wing.common.with_retry.asyncio.sleep", new=fake_sleep):
            asyncio.run(obj.run())

        # base_delay=3 -> 3,6,12,24,48,96,192->clamp 180,180,180,180
        assert sleeps == [3, 6, 12, 24, 48, 96, 180, 180, 180, 180]
        assert all(s <= 180.0 for s in sleeps)

    def test_custom_max_delay_clamps(self):
        """自定义 max_delay 生效。"""
        provider = FakeProvider(config=FakeConfig(max_retries=5, max_retry_delay=10.0))
        obj = _AsyncCallable(provider, fail_times=5)

        sleeps = []

        async def fake_sleep(delay):
            sleeps.append(delay)

        with mock.patch("wing.common.with_retry.asyncio.sleep", new=fake_sleep):
            asyncio.run(obj.run())

        # 3,6,12->clamp 10, 20->clamp 10, 40->clamp 10
        assert sleeps == [3, 6, 10, 10, 10]
        assert all(s <= 10.0 for s in sleeps)


class TestRetryConfigurable:
    """可配置性生效：config 值被装饰器读取。"""

    def test_config_max_retries_used(self):
        """config.max_retries 被装饰器采用。"""
        provider = FakeProvider(config=FakeConfig(max_retries=3, max_retry_delay=180.0))
        obj = _AsyncCallable(provider, fail_times=3)
        with mock.patch("wing.common.with_retry.asyncio.sleep", new=_noop_sleep):
            result = asyncio.run(obj.run())
        assert result == "ok"
        assert obj.calls == 4

    def test_config_max_retries_used_when_passed(self):
        """显式传入 max_retries 时以显式值为准（不读 config）。"""
        provider = FakeProvider(
            config=FakeConfig(max_retries=10, max_retry_delay=180.0)
        )

        class Explicit:
            def __init__(self, provider):
                self._provider = provider
                self.calls = 0

            @with_retry(max_retries=2)
            async def run(self):
                self.calls += 1
                if self.calls <= 2:
                    raise RuntimeError("boom")
                return "ok"

        obj = Explicit(provider)
        with mock.patch("wing.common.with_retry.asyncio.sleep", new=_noop_sleep):
            result = asyncio.run(obj.run())
        assert result == "ok"
        assert obj.calls == 3


class TestConfigFields:
    """ProviderConfig 暴露可配置重试字段。"""

    def test_provider_config_defaults(self):
        from wing.config import ProviderConfig

        cfg = ProviderConfig(
            name="test", base_url="https://api.example.com", api_key="key"
        )
        assert cfg.max_retries == 10
        assert cfg.max_retry_delay == 180.0

    def test_provider_config_custom(self):
        from wing.config import ProviderConfig

        cfg = ProviderConfig(
            name="test",
            base_url="https://api.example.com",
            api_key="key",
            max_retries=5,
            max_retry_delay=60.0,
        )
        assert cfg.max_retries == 5
        assert cfg.max_retry_delay == 60.0
