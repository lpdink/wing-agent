"""model-id 目录（id 空间）测试——``ModelSpec.id`` / 全局唯一 / 解析入口 / 错误文案。

锁定行为（任务书 01 步骤 Verification 的 ①–⑩、⑫）：

- effective id = ``spec.id or spec.name``：存量字符串 / ``{name, display_name}`` 形态
  零改动可用；
- id 跨 provider **全局唯一**（冲突即加载失败，错误含两个 provider 名 + 修复示例）；
- ``providers[].models`` 非空、``agents[].model ∈ id 空间``（错误文案 = C7）；
- id 校验：首尾空白 / 控制字符 / 空串 / 超长拒绝，``:`` ``/`` ``@`` 放行；
- 解析 = **单键查表**：``find_model`` 命中 / 未命中二值（无候选集合、无优先级、无回落）；
- ``identify`` 是反查（旧数据迁移 / 补 id 用），未命中 None；
- ``get_provider`` 必填 name（「默认 provider」概念已消灭）；
- 远端 ``/models`` 发现零残留（ABC / 池都没有对应能力）。
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path
from unittest import mock

import pytest
import yaml
from pydantic import ValidationError

from wing.config import (
    AgentConfig,
    Config,
    ModelGroup,
    ModelRef,
    ModelSpec,
    ProviderConfig,
    load_config,
    reset_config,
)
from wing.provider.base import ModelProvider
from wing.provider.pool import ProviderPool


def _provider(name: str = "p", **kwargs) -> ProviderConfig:
    fields: dict = {"name": name, "base_url": "http://x", "api_key": "k"}
    fields.update(kwargs)
    return ProviderConfig(**fields)


def _config(*providers: ProviderConfig, agent_model: str = "m") -> Config:
    return Config(
        providers=list(providers),
        agents=[AgentConfig(name="default", model=agent_model)],
    )


# ============================================================
# id 声明形态与校验
# ============================================================


class TestModelIdDeclaration:
    """``ModelSpec.id`` 的形态与取值域。"""

    def test_legacy_forms_keep_id_equal_to_name(self):
        """存量两形态（字符串 / {name, display_name}）零改动：effective id = name。"""
        provider = _provider(
            models=["dfmodel", {"name": "sd-x", "display_name": "SD X"}]
        )
        assert provider.model_specs()[0].effective_id == "dfmodel"
        assert provider.model_specs()[1].effective_id == "sd-x"

        config = _config(provider, agent_model="dfmodel")
        assert config.find_model("dfmodel") is not None
        assert config.find_model("sd-x") is not None

    @pytest.mark.parametrize("bad_name", [" x", "x ", " x ", "\ttab"])
    def test_name_with_surrounding_whitespace_rejected(self, bad_name):
        """调用名带首尾空白 → 拒绝（隐式 id 必须无空白，见 `_name_valid`）。

        否则 `effective_id = id or name` 会造出一个带空白的 id，而解析入口
        `find_model()` 按 trim 后的键查表：加载说合法、解析说未知（或静默落到
        另一个模型上）。空白在这里被拒 = id 空间由构造保证干净。
        """
        with pytest.raises(ValidationError, match="leading/trailing whitespace"):
            ModelSpec(name=bad_name)

    def test_agent_model_with_whitespace_loads_and_resolves_consistently(self):
        """`agents[].model` 带首尾空白：「加载说合法 ⇔ 解析能命中」恒成立（S1）。

        校验（`_validate_config`）与解析（`AgentTemplate.from_config`）共用
        `require_model` 的 trim 语义——不会出现「配置加载通过、运行期构造报未知」。
        """
        config = Config(
            providers=[_provider("p", models=["m"])],
            agents=[AgentConfig(name="default", model="  m  ")],
        )
        resolved = config.require_model(config.agents[0].model)
        assert resolved.id == "m"
        assert config.find_model(config.agents[0].model) == resolved

    def test_agent_model_whitespace_variant_misses_like_find_model(self):
        """空白变体只在「trim 后命中」时合法：查不中时加载与解析给同一条错。"""
        with pytest.raises(ValueError, match="unknown model id ' ghost '"):
            Config(
                providers=[_provider("p", models=["m"])],
                agents=[AgentConfig(name="default", model=" ghost ")],
            )

    def test_explicit_id_separates_reference_from_call_name(self):
        """``{id, name}``：id 是引用词，name 是发给上游的调用名。"""
        provider = _provider(
            models=[
                ModelSpec(id="ds-flash", name="dfmodel-2026", display_name="Flash"),
                "plain",
            ]
        )
        config = _config(provider, agent_model="ds-flash")

        ref = config.find_model("ds-flash")
        assert ref is not None
        assert (ref.id, ref.name, ref.provider_name) == (
            "ds-flash",
            "dfmodel-2026",
            "p",
        )
        # 调用名不是引用词：拿 name 查 id 空间必须落空（无回落、无猜测）
        assert config.find_model("dfmodel-2026") is None
        assert config.identify("p", "dfmodel-2026") == "ds-flash"

    @pytest.mark.parametrize(
        "bad_id",
        [
            "",
            "  ",
            " padded",
            "padded ",
            "has\ttab",
            "has\nnewline",
            "x" * 129,
        ],
    )
    def test_invalid_ids_rejected(self, bad_id):
        """首尾空白 / 控制字符 / 空串 / 超长：配置解析期直接拒绝。"""
        with pytest.raises(ValidationError):
            ModelSpec(id=bad_id, name="m")

    @pytest.mark.parametrize("good_id", ["ds-flash", "a:b/c@d", "vendor:model", "a@"])
    def test_visible_characters_allowed(self, good_id):
        """可见字符（含 ``:`` ``/`` ``@``）放行——外部系统的值域不能比我们窄。"""
        assert ModelSpec(id=good_id, name="m").effective_id == good_id

    def test_id_max_length_boundary(self):
        """128 字符是上限（含），129 拒绝。"""
        assert ModelSpec(id="x" * 128, name="m").effective_id == "x" * 128
        with pytest.raises(ValidationError):
            ModelSpec(id="x" * 129, name="m")


# ============================================================
# 配置校验：全局唯一 / 目录非空 / agent.model ∈ id
# ============================================================


class TestCatalogValidation:
    """配置加载期的三条硬约束。"""

    def test_cross_provider_duplicate_id_fails_with_fix_example(self):
        """跨 provider 同名（无显式 id）→ 加载失败；错误含两个 provider 名 + 修复示例。"""
        with pytest.raises(ValueError) as failure:
            Config(
                providers=[
                    _provider("dashscope", models=["gpt-4o"]),
                    _provider("azure", models=["gpt-4o"]),
                ],
                agents=[AgentConfig(name="default", model="gpt-4o")],
            )

        message = str(failure.value)
        assert "duplicate model id 'gpt-4o'" in message
        assert "provider 'dashscope'" in message
        assert "provider 'azure'" in message
        assert "Give one an explicit id, e.g.:" in message
        assert "- id: azure-gpt-4o" in message
        assert "name: gpt-4o" in message

    def test_explicit_id_duplicate_fails_too(self):
        """显式 id 重复同样失败（id 空间与声明形态无关）。"""
        with pytest.raises(ValueError, match="duplicate model id 'shared'"):
            Config(
                providers=[
                    _provider("a", models=[ModelSpec(id="shared", name="m-a")]),
                    _provider("b", models=[ModelSpec(id="shared", name="m-b")]),
                ],
                agents=[AgentConfig(name="default", model="shared")],
            )

    def test_duplicate_id_within_one_provider_names_it_once(self):
        """同一 provider 内两条声明撞 id：错误信息只说一次 provider（N4）。

        跨 provider 的模板（"... provider 'a' and provider 'b'"）落到单 provider
        场景会读成 bug（'a' and 'a'）；这里换成 "declared twice by provider 'a'"，修复示例不变。
        """
        with pytest.raises(ValueError) as failure:
            Config(
                providers=[
                    _provider("a", models=["x", ModelSpec(id="x", name="y")]),
                ],
                agents=[AgentConfig(name="default", model="x")],
            )

        message = str(failure.value)
        assert "duplicate model id 'x' declared twice by provider 'a'." in message
        assert "'a' and provider 'a'" not in message
        assert "Give one an explicit id, e.g.:" in message
        assert "- id: a-y" in message

    def test_same_name_across_providers_is_fine_with_distinct_ids(self):
        """跨 provider 同名**调用名**合法：只要 id 不同（各自可切换）。"""
        config = Config(
            providers=[
                _provider("a", models=["shared-name"]),
                _provider("b", models=[ModelSpec(id="b-shared", name="shared-name")]),
            ],
            agents=[AgentConfig(name="default", model="shared-name")],
        )
        by_name = config.find_model("shared-name")
        by_explicit_id = config.find_model("b-shared")
        assert by_name is not None and by_name.provider_name == "a"
        assert by_explicit_id is not None and by_explicit_id.provider_name == "b"
        assert config.identify("a", "shared-name") == "shared-name"
        assert config.identify("b", "shared-name") == "b-shared"

    def test_provider_without_models_fails(self):
        """providers[].models 空 → 加载失败（目录只能靠声明，没有远端兜底）。"""
        with pytest.raises(ValueError, match="provider 'a' declares no models"):
            Config(
                providers=[_provider("a", models=[])],
                agents=[AgentConfig(name="default", model="m")],
            )

    def test_agent_model_outside_id_space_fails(self):
        """agents[].model 不是任何 id → 加载失败，文案 = C7（available + 提示）。"""
        with pytest.raises(ValueError) as failure:
            Config(
                providers=[
                    _provider("local", models=[ModelSpec(id="ds-flash", name="sonnet")])
                ],
                agents=[AgentConfig(name="default", model="sonnet")],
            )

        message = str(failure.value)
        assert "unknown model id 'sonnet'" in message
        assert "available ids: ds-flash" in message
        assert "'sonnet' is the call name of model id 'ds-flash'" in message
        assert "declare an explicit id or send 'ds-flash'" in message

    def test_agents_provider_field_is_ignored_not_validated(self):
        """``agents[].provider`` 已删除：写了不报错、不做引用校验。"""
        config = Config(
            providers=[_provider("a", models=["m"])],
            agents=[
                AgentConfig(name="default", model="m", provider="ghost")  # ty: ignore[unknown-argument]
            ],
        )
        assert config.agents[0].model == "m"


# ============================================================
# 解析入口：find_model / identify / model_groups / get_provider
# ============================================================


class TestResolveEntryPoints:
    """解析与反查的唯一入口。"""

    @pytest.fixture
    def catalog(self) -> Config:
        return Config(
            providers=[
                _provider(
                    "local",
                    models=[
                        "dfmodel",
                        ModelSpec(
                            id="ds-flash",
                            name="sonnet",
                            display_name="DeepSeek-Flash",
                            description="fast",
                        ),
                    ],
                ),
                _provider(
                    "remote", models=[ModelSpec(id="gpt-4o", name="gpt-4o-2026")]
                ),
            ],
            agents=[AgentConfig(name="default", model="dfmodel")],
        )

    def test_find_model_hits_and_misses(self, catalog):
        """命中返回 ModelRef；未命中 None——没有候选集合、没有回落。"""
        ref = catalog.find_model("dfmodel")
        assert isinstance(ref, ModelRef)
        assert (ref.id, ref.name, ref.provider_name) == ("dfmodel", "dfmodel", "local")
        assert isinstance(ref.spec, ModelSpec)

        flash = catalog.find_model("ds-flash")
        assert flash is not None
        assert flash.name == "sonnet"
        assert flash.spec.display_name == "DeepSeek-Flash"

        assert catalog.find_model("ds-flas") is None  # 不做前缀匹配
        assert catalog.find_model("") is None
        # 目录序不影响解析：id 在全域唯一，命中的就是唯一那一条
        remote = catalog.find_model("gpt-4o")
        assert remote is not None and remote.provider_name == "remote"

    def test_find_model_trims_input(self, catalog):
        """输入先 trim（id 无首尾空白，按「引用词」查仍然唯一确定）。"""
        ref = catalog.find_model("  dfmodel  ")
        assert ref is not None and ref.id == "dfmodel"

    def test_identify_reverse_lookup(self, catalog):
        """(provider, name) → effective id；未命中 None（换 provider / 换名都落空）。"""
        assert catalog.identify("local", "sonnet") == "ds-flash"
        assert catalog.identify("local", "dfmodel") == "dfmodel"
        assert catalog.identify("remote", "sonnet") is None
        assert catalog.identify("local", "nope") is None
        assert catalog.identify("ghost", "dfmodel") is None

    def test_require_model_raises_c7_message(self, catalog):
        """``require_model`` 是 raise 侧统一入口（文案与配置校验同源）。"""
        assert catalog.require_model("dfmodel").id == "dfmodel"
        with pytest.raises(ValueError, match="unknown model id 'nope'"):
            catalog.require_model("nope")

    def test_unknown_message_truncates_available_ids(self):
        """available ids 超过 10 个截断为前 10 + ``…``。"""
        config = _config(
            _provider("p", models=[f"m{i}" for i in range(12)]), agent_model="m0"
        )
        message = config.describe_unknown_model("ghost")
        assert "available ids: m0, m1, m2, m3, m4, m5, m6, m7, m8, m9, …" in message
        assert "m10" not in message

    def test_unknown_message_lists_every_name_hint(self):
        """同名调用名跨 provider：提示列出全部候选 id（人自己挑，代码不猜）。"""
        config = Config(
            providers=[
                _provider("a", models=[ModelSpec(id="a-shared", name="shared")]),
                _provider("b", models=[ModelSpec(id="b-shared", name="shared")]),
            ],
            agents=[AgentConfig(name="default", model="a-shared")],
        )
        message = config.describe_unknown_model("shared")
        assert (
            "is the call name of model ids "
            "'a-shared' (provider 'a') and 'b-shared' (provider 'b')" in message
        )
        assert "send one of those ids" in message

    def test_model_groups_follow_declaration_order(self, catalog):
        """目录分组视图 = 配置声明序（provider 序 × 模型序），条目类型是 ModelRef。"""
        groups = catalog.model_groups()
        assert [g.provider for g in groups] == ["local", "remote"]
        assert all(isinstance(g, ModelGroup) for g in groups)
        assert [ref.id for ref in groups[0].models] == ["dfmodel", "ds-flash"]
        assert [ref.name for ref in groups[0].models] == ["dfmodel", "sonnet"]
        assert groups[1].models[0].id == "gpt-4o"

    def test_get_provider_requires_name(self, catalog):
        """``get_provider`` 必填 name：None / 空串 / 未知都不回落第一个 provider。"""
        assert catalog.get_provider("remote").name == "remote"
        with pytest.raises(ValueError, match="provider name is required"):
            catalog.get_provider(None)  # 非 str 入参：运行期闸门（None 不再回落第一个）
        with pytest.raises(ValueError, match="provider name is required"):
            catalog.get_provider("")
        with pytest.raises(ValueError, match="provider 'ghost' not found"):
            catalog.get_provider("ghost")


# ============================================================
# 存量配置零改动 + 远端发现零残留
# ============================================================


class TestLegacyConfigStillLoads:
    """存量配置（声明里没有 id）经真实加载路径零改动可加载。"""

    @pytest.fixture(autouse=True)
    def _reset(self):
        reset_config()
        yield
        reset_config()

    def test_legacy_config_file_loads(self):
        legacy = {
            "providers": [
                {
                    "name": "default",
                    "base_url": "https://api.example.com/v1",
                    "api_key": "k",
                    "models": ["dfmodel", {"name": "sd-x", "display_name": "SD X"}],
                }
            ],
            "agents": [
                {
                    "name": "default",
                    "model": "dfmodel",
                    # 已删除字段：用户继续写，静默忽略
                    "provider": "default",
                }
            ],
        }
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(legacy, f)
            temp_path = f.name
        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                config = load_config()
        finally:
            os.unlink(temp_path)

        declared = config.find_model("sd-x")
        assert declared is not None and declared.name == "sd-x"
        assert config.agents[0].model == "dfmodel"

    def test_cross_provider_legacy_collision_fails_at_load(self):
        """存量配置里的隐性冲突（两个 provider 都只写裸名）现在会加载失败。"""
        collision = {
            "providers": [
                {"name": "a", "base_url": "http://a", "api_key": "k", "models": ["m"]},
                {"name": "b", "base_url": "http://b", "api_key": "k", "models": ["m"]},
            ],
            "agents": [{"name": "default", "model": "m"}],
        }
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(collision, f)
            temp_path = f.name
        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                with pytest.raises(ValueError, match="duplicate model id 'm'"):
                    load_config()
        finally:
            os.unlink(temp_path)


class TestRemoteDiscoveryIsGone:
    """远端 ``/models`` 发现零残留（能力与调用点都不存在了）。"""

    def test_provider_abc_has_no_list_models(self):
        assert not hasattr(ModelProvider, "list_models")

    def test_pool_has_no_catalog_api(self):
        assert not hasattr(ProviderPool, "list_all_models")
        assert not hasattr(ProviderPool, "_configs")

    def test_protocol_provider_implementations_have_no_list_models(self):
        from wing.provider.anthropic.provider import AnthropicProvider
        from wing.provider.openai.provider import OpenAICompatProvider

        assert not hasattr(OpenAICompatProvider, "list_models")
        assert not hasattr(AnthropicProvider, "list_models")
