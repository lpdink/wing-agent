"""Provider registry 测试——模块级持有所有 provider client，聚合模型列表。

锁定行为：
- 配置了静态 models 的 provider 跳过远端请求（provider.list_models 内部契约）
- 单个 provider 查询失败落空，不影响其余
- reset 关闭全部 client 并清表（config reload 入口）
"""

from __future__ import annotations

import pytest

import wing.provider as provider_pkg
from wing.config import AgentConfig, Config, ProviderConfig
from wing.provider import ProviderModels, _ProviderRegistry


class _FakeProvider:
    def __init__(self, name: str, models: list[str] | None = None, fail: bool = False):
        self._name = name
        self._models = models or []
        self._fail = fail
        self.closed = False
        self.list_called = False

    @property
    def name(self) -> str:
        return self._name

    async def list_models(self) -> list[str]:
        self.list_called = True
        if self._fail:
            raise RuntimeError("API error")
        return self._models

    async def aclose(self) -> None:
        self.closed = True


def _config(*names: str) -> Config:
    return Config(
        providers=[
            ProviderConfig(name=n, base_url="http://x", api_key="k") for n in names
        ],
        agents=[AgentConfig(name="default", model="m", provider=names[0])],
    )


class TestProviderRegistry:
    @pytest.mark.asyncio
    async def test_aggregates_grouped_by_provider(self, monkeypatch):
        fakes = {
            "p1": _FakeProvider("p1", ["m1", "m2"]),
            "p2": _FakeProvider("p2", ["m3"]),
        }
        monkeypatch.setattr("wing.config.get_config", lambda: _config("p1", "p2"))
        monkeypatch.setattr(
            provider_pkg, "create_provider", lambda cfg: fakes[cfg.name]
        )

        registry = _ProviderRegistry()
        result = await registry.list_all_models()

        assert {(g.provider, tuple(g.models)) for g in result} == {
            ("p1", ("m1", "m2")),
            ("p2", ("m3",)),
        }

    @pytest.mark.asyncio
    async def test_single_failure_falls_empty(self, monkeypatch):
        """单个 provider 失败落空（模型列表为空），不影响其余。"""
        fakes = {
            "ok": _FakeProvider("ok", ["m1"]),
            "bad": _FakeProvider("bad", fail=True),
        }
        monkeypatch.setattr("wing.config.get_config", lambda: _config("ok", "bad"))
        monkeypatch.setattr(
            provider_pkg, "create_provider", lambda cfg: fakes[cfg.name]
        )

        result = await _ProviderRegistry().list_all_models()
        by_name = {g.provider: g.models for g in result}
        assert by_name["ok"] == ["m1"]
        assert by_name["bad"] == []

    @pytest.mark.asyncio
    async def test_static_models_skip_request(self, monkeypatch):
        """配置了静态 models 的 provider 跳过远端请求（list_models 内部短路）。"""
        cfg = Config(
            providers=[
                ProviderConfig(
                    name="static", base_url="http://x", api_key="k", models=["a", "b"]
                )
            ],
            agents=[AgentConfig(name="default", model="m", provider="static")],
        )
        monkeypatch.setattr("wing.config.get_config", lambda: cfg)
        # 不 mock create_provider——真实 provider 的 list_models 见静态 models
        # 直接返回，不发 HTTP（base_url 不可达也不会报错，证明未请求）
        result = await _ProviderRegistry().list_all_models()
        assert result == [ProviderModels(provider="static", models=["a", "b"])]

    @pytest.mark.asyncio
    async def test_reset_closes_all_clients(self, monkeypatch):
        fakes = {"p1": _FakeProvider("p1", ["m1"])}
        monkeypatch.setattr("wing.config.get_config", lambda: _config("p1"))
        monkeypatch.setattr(
            provider_pkg, "create_provider", lambda cfg: fakes[cfg.name]
        )

        registry = _ProviderRegistry()
        await registry.list_all_models()
        await registry.reset()

        assert fakes["p1"].closed is True
        assert registry._providers == {}

    @pytest.mark.asyncio
    async def test_clients_held_across_queries(self, monkeypatch):
        """client 长持有：多次查询复用同一实例（非每请求临时创建）。"""
        created: list[_FakeProvider] = []

        def _create(cfg):
            p = _FakeProvider(cfg.name, ["m"])
            created.append(p)
            return p

        monkeypatch.setattr("wing.config.get_config", lambda: _config("p1"))
        monkeypatch.setattr(provider_pkg, "create_provider", _create)

        registry = _ProviderRegistry()
        await registry.list_all_models()
        await registry.list_all_models()
        assert len(created) == 1
