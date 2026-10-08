"""共享 provider 池（``wing.provider.pool``）单元测试。

被守的语义（#172 的架构面）：

- 每 name 一个实例（懒建）；同名跨会话共享——FD / 连接池不再按会话放大；
- ``reset`` = 先建后换：任一构建失败池保持原样；成功则换新并 retire 旧实例；
- ``retire`` **不打断在途请求**：在途计数归零才真正关闭（重试栈绝不绑死
  在已关闭的 client 上——reload 在途调用的冲突根因）；
- 已关闭实例上的新调用快速失败（`ProviderClosedError`，不进重试栈空转）；
- 配置移除的 name 保留旧实例（钉在它上面的会话不因 reload 被拆）；
- ``/api/models`` 聚合复用同一批实例（不再维护第二套只读 client）。
"""

from __future__ import annotations

import asyncio

import pytest

from wing.config import AgentConfig, Config, ModelSpec, ProviderConfig
from wing.provider.base import ModelProvider, ProviderClosedError
from wing.provider.pool import ProviderPool
from wing.schema import LLMResponse, Message


class _FailingProvider(ModelProvider):
    """list_models 恒失败的受控 provider（聚合失败落空的测试桩）。"""

    def __init__(self, name: str) -> None:
        super().__init__()
        self._config = ProviderConfig(name=name, base_url="http://x", api_key="k")

    def _generate(self, *args, **kwargs):
        raise NotImplementedError

    async def list_models(self) -> list[str]:
        raise RuntimeError("boom")


class _ControlledProvider(ModelProvider):
    """可控在途窗口的最小 provider（协议实现无关）。"""

    def __init__(self, name: str) -> None:
        super().__init__()
        self._config = ProviderConfig(name=name, base_url="http://x", api_key="k")
        self.release = asyncio.Event()
        self.transport_closed = 0
        self.list_calls = 0

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

    async def list_models(self) -> list[str]:
        self.list_calls += 1
        return ["m1"]


def _config(*names: str) -> Config:
    return Config(
        providers=[
            ProviderConfig(name=n, base_url="http://x", api_key="k") for n in names
        ],
        agents=[AgentConfig(name="default", model="m", provider=names[0])],
    )


def _install(pool: ProviderPool, *providers: ModelProvider) -> None:
    """把受控 provider 登记进池（替换懒建条目，测试自建实例的注入口径）。"""
    for provider in providers:
        pool._providers[provider.name] = provider
        pool._configs[provider.name] = provider.config


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


class TestListAllModels:
    @pytest.mark.asyncio
    async def test_aggregates_with_shared_instances(self, monkeypatch):
        """聚合查询复用池实例本身（不再有第二套只读 client）。"""
        monkeypatch.setattr("wing.config.loader._config", _config("default", "alt"))
        pool = ProviderPool()
        first, second = _ControlledProvider("default"), _ControlledProvider("alt")
        _install(pool, first, second)

        groups = await pool.list_all_models()

        assert sorted(g.provider for g in groups) == ["alt", "default"]
        assert all(g.models == ["m1"] for g in groups)
        # 查询确实打在共享实例上（同一对象计数），而不是新建的 client
        assert first.list_calls == 1
        assert second.list_calls == 1

    @pytest.mark.asyncio
    async def test_close_clears_and_closes_all(self, monkeypatch):
        monkeypatch.setattr("wing.config.loader._config", _config("default", "alt"))
        pool = ProviderPool()
        a, b = pool.get("default"), pool.get("alt")

        await pool.close()

        assert a._closed is True
        assert b._closed is True
        assert pool._providers == {}

    @pytest.mark.asyncio
    async def test_single_failure_falls_back_to_empty_group(self, monkeypatch):
        """单个 provider 查询失败落空，不影响其余分组（registry 时代的既有契约）。"""
        monkeypatch.setattr("wing.config.loader._config", _config("default", "alt"))
        pool = ProviderPool()
        _install(pool, _FailingProvider("default"), _ControlledProvider("alt"))

        groups = await pool.list_all_models()

        by_name = {group.provider: group for group in groups}
        assert by_name["default"].models == []
        assert by_name["alt"].models == ["m1"]

    @pytest.mark.asyncio
    async def test_details_align_with_static_declarations(self, monkeypatch):
        """声明的模型带元信息（display_name / description）；details 与 models 逐项同序同名。"""
        cfg = Config(
            providers=[
                ProviderConfig(
                    name="default",
                    base_url="http://x",
                    api_key="k",
                    models=[
                        ModelSpec(name="fancy", display_name="Fancy", description="d"),
                        "plain",
                    ],
                )
            ],
            agents=[AgentConfig(name="default", model="fancy", provider="default")],
        )
        monkeypatch.setattr("wing.config.loader._config", cfg)
        pool = ProviderPool()

        (group,) = await pool.list_all_models()

        assert group.models == ["fancy", "plain"]
        assert [detail.name for detail in group.model_details] == group.models
        assert group.model_details[0].display_name == "Fancy"
        assert group.model_details[0].description == "d"
        assert group.model_details[1].display_name is None  # 远端发现 / 裸名最小条目


async def _collect(provider: ModelProvider, sink: list[LLMResponse]) -> None:
    async for chunk in provider.generate([Message(role="user", content="q")], "m"):
        sink.append(chunk)
