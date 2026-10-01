"""模型声明协议测试——``models: str | ModelSpec``、能力解析、两 provider list_models。

锁定行为：

- 存量字符串形态零修改可用；对象形态携带 display_name / description / capabilities；
- 未声明能力 = 全 false（安全默认，不做名字启发式）；
- 同一 provider 内实际调用名不得重复（对象/字符串混排也查）；
- ``image_max_bytes`` 与 ``images.*`` 非法值在解析期报错；
- ``list_models`` 对两种形态返回排序后的实际调用名（静态声明短路，无网络）。
"""

from __future__ import annotations

import pytest
from pydantic import ValidationError

from wing.config import (
    AgentConfig,
    Config,
    ImagesConfig,
    ModelCapabilities,
    ModelSpec,
    ProviderConfig,
    resolve_model_capabilities,
    resolve_model_display_name,
)
from wing.provider.anthropic import AnthropicProvider
from wing.provider.openai_compat import OpenAICompatProvider


def _provider(**kwargs) -> ProviderConfig:
    """最小合法 provider 配置（可覆盖任意字段）。"""
    fields: dict = {"name": "p", "base_url": "http://x", "api_key": "k"}
    fields.update(kwargs)
    return ProviderConfig(**fields)


def _config(provider: ProviderConfig) -> Config:
    return Config(
        providers=[provider],
        agents=[AgentConfig(name="default", model="m", provider=provider.name)],
    )


class TestProviderModelDeclarations:
    """``ProviderConfig.models`` 两种形态的解析与访问口。"""

    def test_legacy_string_models_unchanged(self):
        cfg = _provider(models=["a", "b"])
        assert cfg.model_names() == ["a", "b"]
        assert cfg.find_model("a") == ModelSpec(name="a")

    def test_object_form_carries_metadata(self):
        cfg = _provider(
            models=[
                {
                    "name": "dfmodel-2026",
                    "display_name": "DeepSeek-Flash",
                    "description": "deepseek official release",
                    "capabilities": {"vision": True},
                }
            ]
        )
        spec = cfg.find_model("dfmodel-2026")
        assert spec is not None
        assert spec.name == "dfmodel-2026"
        assert spec.display_name == "DeepSeek-Flash"
        assert spec.description == "deepseek official release"
        assert spec.capabilities.vision is True

    def test_object_form_defaults_and_ignores_unknown_keys(self):
        """对象缺省字段回落；未知键忽略（向前兼容未来扩展字段）。"""
        cfg = _provider(
            models=[{"name": "x", "future_field": 1, "capabilities": {"audio": True}}]
        )
        spec = cfg.find_model("x")
        assert spec is not None
        assert spec.display_name is None
        assert spec.description is None
        assert spec.capabilities == ModelCapabilities(vision=False)

    def test_mixed_forms_preserve_declaration_order(self):
        cfg = _provider(models=["legacy", {"name": "new", "display_name": "New"}])
        assert cfg.model_names() == ["legacy", "new"]

    def test_find_model_unknown_returns_none(self):
        cfg = _provider(models=["a"])
        assert cfg.find_model("b") is None
        assert _provider().find_model("a") is None

    @pytest.mark.parametrize(
        "models",
        [
            ["a", "a"],
            [{"name": "a"}, {"name": "a"}],
            ["a", {"name": "a"}],
            [{"name": "a"}, "a"],
        ],
    )
    def test_duplicate_model_names_rejected(self, models):
        with pytest.raises(ValidationError, match="duplicate model name"):
            _provider(models=models)

    def test_empty_model_name_rejected(self):
        with pytest.raises(ValidationError):
            _provider(models=[{"name": "  "}])

    def test_image_max_bytes_must_be_positive(self):
        assert _provider(image_max_bytes=5_242_880).image_max_bytes == 5_242_880
        assert _provider(image_max_bytes=None).image_max_bytes is None
        with pytest.raises(ValidationError, match="image_max_bytes"):
            _provider(image_max_bytes=0)
        with pytest.raises(ValidationError, match="image_max_bytes"):
            _provider(image_max_bytes=-1)

    def test_image_delivery_literal(self):
        assert _provider().image_delivery is None
        assert _provider(image_delivery="inline").image_delivery == "inline"
        assert _provider(image_delivery="followup").image_delivery == "followup"
        with pytest.raises(ValidationError):
            _provider(image_delivery="sideways")

    def test_defaults(self):
        cfg = _provider()
        assert cfg.models == []
        assert cfg.image_delivery is None
        assert cfg.image_max_bytes is None


class TestResolveModelCapabilities:
    """能力解析：唯一入口是显式声明。"""

    def test_declared_vision_true(self):
        cfg = _provider(models=[{"name": "v", "capabilities": {"vision": True}}])
        assert resolve_model_capabilities(cfg, "v").vision is True

    def test_declared_vision_false(self):
        cfg = _provider(models=[{"name": "t", "capabilities": {"vision": False}}])
        assert resolve_model_capabilities(cfg, "t").vision is False

    def test_undeclared_model_is_text_only(self):
        """不在列表内 / 字符串形态 / 空列表——一律全 false（安全默认）。"""
        cfg = _provider(models=["legacy"])
        assert resolve_model_capabilities(cfg, "legacy").vision is False
        assert resolve_model_capabilities(cfg, "other").vision is False
        assert resolve_model_capabilities(_provider(), "any").vision is False

    @pytest.mark.parametrize("model", ["gpt-4o", "qwen-vl-max", "claude-3-vision"])
    def test_no_name_heuristic(self, model):
        """名字带 vl / vision / 4o 也不启发式放行。"""
        assert resolve_model_capabilities(_provider(), model).vision is False


class TestResolveModelDisplayName:
    """展示名解析：声明有空判、未声明回落 None（前端回落调用名）。"""

    def test_declared_display_name(self):
        cfg = _provider(models=[{"name": "dfmodel", "display_name": "DeepSeek-Flash"}])
        assert resolve_model_display_name(cfg, "dfmodel") == "DeepSeek-Flash"

    def test_string_form_has_no_display_name(self):
        cfg = _provider(models=["legacy"])
        assert resolve_model_display_name(cfg, "legacy") is None

    def test_undeclared_or_missing_display_name_falls_back_to_none(self):
        cfg = _provider(
            models=[{"name": "bare"}, {"name": "empty", "display_name": ""}]
        )
        assert resolve_model_display_name(cfg, "bare") is None
        assert resolve_model_display_name(cfg, "empty") is None
        assert resolve_model_display_name(cfg, "other") is None
        assert resolve_model_display_name(_provider(), "any") is None

    def test_blank_display_name_is_no_display_name(self):
        """纯空白与空串等价（前端同样按空白回落）。"""
        cfg = _provider(models=[{"name": "spaces", "display_name": "   "}])
        assert resolve_model_display_name(cfg, "spaces") is None

    def test_no_name_heuristic(self):
        """没有声明就不猜展示名（与能力解析同一口径）。"""
        assert resolve_model_display_name(_provider(), "DeepSeek-Flash") is None


class TestImagesConfig:
    """``Config.images`` 缺省值与校验。"""

    def test_defaults(self):
        provider = _provider()
        assert _config(provider).images == ImagesConfig(
            max_bytes=4_718_592,
            max_images=32,
            count_quantum=8,
            request_budget_bytes=37_748_736,
            evict_quantum_bytes=18_874_368,
        )

    def test_overrides(self):
        """覆盖字段生效、未覆盖字段回落默认。

        dict → ImagesConfig 的强制转换由模板测试（真实 YAML 解析）覆盖，
        此处用显式对象（ty 门禁不接受裸 dict 传给 pydantic 字段）。
        """
        config = Config(
            providers=[_provider()],
            agents=[AgentConfig(name="default", model="m", provider="p")],
            images=ImagesConfig(max_bytes=1024, max_images=2),
        )
        assert config.images.max_bytes == 1024
        assert config.images.max_images == 2
        assert config.images.count_quantum == 8  # 未覆盖字段回落默认

    @pytest.mark.parametrize(
        "field",
        [
            "max_bytes",
            "max_images",
            "count_quantum",
            "request_budget_bytes",
            "evict_quantum_bytes",
        ],
    )
    @pytest.mark.parametrize("value", [0, -1])
    def test_non_positive_values_rejected(self, field, value):
        with pytest.raises(ValidationError):
            ImagesConfig(**{field: value})


class TestProviderListModelsStaticDeclarations:
    """静态声明短路：两种形态都返回排序后的实际调用名（无网络请求）。"""

    @pytest.mark.asyncio
    async def test_openai_provider_mixed_forms(self):
        provider = OpenAICompatProvider(
            config=_provider(models=["b-string", {"name": "a-object"}])
        )
        try:
            assert await provider.list_models() == ["a-object", "b-string"]
        finally:
            await provider.aclose()

    @pytest.mark.asyncio
    async def test_anthropic_provider_mixed_forms(self):
        provider = AnthropicProvider(
            config=_provider(
                protocol="anthropic", models=[{"name": "b-object"}, "a-string"]
            )
        )
        try:
            assert await provider.list_models() == ["a-string", "b-object"]
        finally:
            await provider.aclose()
