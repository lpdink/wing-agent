"""共享 provider 池（``wing.provider.pool``）单元测试。

被守的语义（#172 的架构面）：

- 每 name 一个实例（懒建）；同名跨会话共享——FD / 连接池不再按会话放大；
- ``reset`` = 先建后换：任一构建失败池保持原样；成功则换新并 retire 旧实例；
- ``retire`` **不打断在途请求**：在途计数归零才真正关闭（重试栈绝不绑死
  在已关闭的 client 上——reload 在途调用的冲突根因）；
- 已关闭实例上的新调用快速失败（`ProviderClosedError`，不进重试栈空转）；
- 配置移除的 name 保留旧实例（钉在它上面的会话不因 reload 被拆）；
- 池只持有实例：模型目录已归配置声明（``/api/models`` 不再经池聚合）。
"""

from __future__ import annotations

import asyncio

import pytest

from wing.config import AgentConfig, Config, ProviderConfig
from wing.provider.base import ModelProvider, ProviderClosedError
from wing.provider.pool import ProviderPool
from wing.schema import LLMResponse, Message


class _ControlledProvider(ModelProvider):
    """可控在途窗口的最小 provider（协议实现无关）。"""

    def __init__(self, name: str) -> None:
        super().__init__()
        self._config = ProviderConfig(name=name, base_url="http://x", api_key="k")
        self.release = asyncio.Event()
        self.transport_closed = 0

    async def _generate(
        self,
        messages,
        model,
        tools=None,
        stream=False,
        accumulator=None,
        options=None,
    ):
        await self.release.wait()
        yield LLMResponse(content="done")

    async def _close_transport(self) -> None:
        self.transport_closed += 1


def _config(*names: str) -> Config:
    """每个 provider 声明一个模型 id（配置契约：目录来自声明且 id 全局唯一）。"""
    return Config(
        providers=[
            ProviderConfig(name=n, base_url="http://x", api_key="k", models=[f"{n}-m"])
            for n in names
        ],
        agents=[AgentConfig(name="default", model=f"{names[0]}-m")],
    )


def _install(pool: ProviderPool, *providers: ModelProvider) -> None:
    """把受控 provider 登记进池（替换懒建条目，测试自建实例的注入口径）。"""
    for provider in providers:
        pool._providers[provider.name] = provider


async def _wait_inflight(provider: ModelProvider, expected: int = 1) -> None:
    """等到在途计数到达期望值（在途请求已真正挂起）。"""
    for _ in range(200):
        if provider._inflight == expected:
            return
        await asyncio.sleep(0)
    raise AssertionError(f"inflight never reached {expected}")


class TestPoolResolve:
    @pytest.mark.asyncio
    async def test_get_is_lazy_and_shared(self, monkeypatch):
        monkeypatch.setattr("wing.config.loader._config", _config("default"))
        pool = ProviderPool()
        assert pool._providers == {}  # 懒建：get 之前零实例

        p1 = pool.get("default")
        p2 = pool.get("default")
        assert p1 is p2
        assert isinstance(p1, ModelProvider)

    @pytest.mark.asyncio
    async def test_get_unknown_name_raises(self, monkeypatch):
        monkeypatch.setattr("wing.config.loader._config", _config("default"))
        pool = ProviderPool()
        with pytest.raises(ValueError, match="provider 'nope' not found"):
            pool.get("nope")

    @pytest.mark.asyncio
    async def test_closed_instance_refuses_new_calls(self, monkeypatch):
        """关闭后的实例被用于发起新调用 → 快速失败（不进重试栈空转）。"""
        monkeypatch.setattr("wing.config.loader._config", _config("default"))
        pool = ProviderPool()
        provider = pool.get("default")
        await provider.aclose()

        with pytest.raises(ProviderClosedError):
            async for _ in provider.generate([Message(role="user", content="q")], "m"):
                pass  # pragma: no cover


class TestRetireSemantics:
    """退场 = 不打断在途、归零即关（reload 对在途透明的实现基础）。"""

    @pytest.mark.asyncio
    async def test_retire_closes_idle_instance_immediately(self):
        provider = _ControlledProvider("p")
        await provider.retire()
        assert provider._closed is True
        assert provider.transport_closed == 1

    @pytest.mark.asyncio
    async def test_retire_waits_for_inflight_request(self):
        """在途请求跑完之前绝不关闭 client；归零后自动关闭。"""
        provider = _ControlledProvider("p")
        chunks: list[LLMResponse] = []

        task = asyncio.create_task(_collect(provider, chunks))
        await _wait_inflight(provider)

        await provider.retire()
        assert provider._closed is False  # 在途未归零：退场只是记账
        assert provider.transport_closed == 0

        provider.release.set()
        await asyncio.wait_for(task, timeout=1.0)
        assert chunks and chunks[-1].content == "done"
        assert provider._closed is True  # 归零后由收尾路径关闭
        assert provider.transport_closed == 1


class TestPoolReset:
    @pytest.mark.asyncio
    async def test_reset_rebuilds_and_retires_idle_instances(self, monkeypatch):
        monkeypatch.setattr("wing.config.loader._config", _config("default", "alt"))
        pool = ProviderPool()
        old = pool.get("default")

        rebuilt = await pool.reset()

        assert rebuilt == 2
        assert pool.get("default") is not old
        assert old._closed is True  # 无在途 → 退场即关
        assert pool.get("default")._closed is False

    @pytest.mark.asyncio
    async def test_reset_during_inflight_keeps_request_alive(self, monkeypatch):
        """reset（reload）落在在途请求上：请求照常完成，旧实例收尾后关闭。"""
        monkeypatch.setattr("wing.config.loader._config", _config("default"))
        pool = ProviderPool()
        provider = _ControlledProvider("default")
        _install(pool, provider)

        chunks: list[LLMResponse] = []
        task = asyncio.create_task(_collect(provider, chunks))
        await _wait_inflight(provider)

        await pool.reset()
        assert pool.get("default") is not provider  # 池已换新

        assert provider._closed is False  # 在途在身：不关闭
        provider.release.set()
        await asyncio.wait_for(task, timeout=1.0)
        assert chunks[-1].content == "done"
        assert provider._closed is True

    @pytest.mark.asyncio
    async def test_reset_failure_keeps_pool_intact(self, monkeypatch):
        """先建后换：任一 provider 构建失败，整个池保持原样。"""
        monkeypatch.setattr("wing.config.loader._config", _config("default"))

        import wing.provider.pool as pool_mod

        pool = ProviderPool()
        old = pool.get("default")

        def _boom(cfg):
            raise RuntimeError("boom")

        monkeypatch.setattr(pool_mod, "create_provider", _boom)
        with pytest.raises(RuntimeError, match="boom"):
            await pool.reset()

        assert pool.get("default") is old
        assert old._closed is False

    @pytest.mark.asyncio
    async def test_removed_name_stays_resolvable(self, monkeypatch):
        """配置移除的 name：池保留旧实例——钉住它的会话不被 reload 拆解。"""
        monkeypatch.setattr("wing.config.loader._config", _config("default", "alt"))
        pool = ProviderPool()
        alt = pool.get("alt")

        monkeypatch.setattr("wing.config.loader._config", _config("default"))
        await pool.reset()

        assert pool.get("alt") is alt
        assert alt._closed is False


class TestPoolClose:
    @pytest.mark.asyncio
    async def test_close_clears_and_closes_all(self, monkeypatch):
        monkeypatch.setattr("wing.config.loader._config", _config("default", "alt"))
        pool = ProviderPool()
        a, b = pool.get("default"), pool.get("alt")

        await pool.close()

        assert a._closed is True
        assert b._closed is True
        assert pool._providers == {}


async def _collect(provider: ModelProvider, sink: list[LLMResponse]) -> None:
    async for chunk in provider.generate([Message(role="user", content="q")], "m"):
        sink.append(chunk)
