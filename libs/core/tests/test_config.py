# tests/test_config.py
"""配置模块单元测试"""

import os
import tempfile
from pathlib import Path
from unittest import mock

import pytest
import yaml

from wing.config import (
    Config,
    ProviderConfig,
    get_config,
    get_config_path,
    get_wing_home,
    load_config,
    reset_config,
)


@pytest.fixture(autouse=True)
def reset_config_before_each_test():
    """每个测试前重置配置缓存"""
    reset_config()
    yield
    reset_config()


@pytest.fixture
def minimal_config_dict() -> dict:
    """最小配置字典（仅必需字段 + 模型目录的一条声明）。"""
    return {
        "providers": [
            {
                "name": "default",
                "base_url": "https://api.example.com/v1",
                "api_key": "test-key-123",
                "models": ["gpt-4"],
            },
        ],
        "agents": [
            {"name": "default", "model": "gpt-4"},
        ],
    }


@pytest.fixture
def full_config_dict() -> dict:
    """完整配置字典。

    ``agents[].provider`` 是**已删除**的字段（引用词是 model id）：配置里继续写
    它不会报错，也不会出现在解析产物上——``extra="ignore"`` 的静默忽略就是契约。
    """
    return {
        "providers": [
            {
                "name": "main",
                "protocol": "openai",
                "base_url": "https://api.example.com/v1",
                "api_key": "test-key-123",
                "timeout_first_chunk": 120.0,
                "explicit_cache_mode": False,
                "reasoning_effort": "high",
                "extra_body": {"enable_thinking": True},
                "models": [{"name": "gpt-4", "display_name": "GPT-4"}],
            },
        ],
        "agents": [
            {
                "name": "main",
                "model": "gpt-4",
                "provider": "main",
                "system_prompt": "You are a helpful assistant.",
                "tools": ["Bash", "Read", "Write"],
                "context_window_tokens": 50000,
            },
        ],
    }


class TestConfigModels:
    """测试 Pydantic 模型"""

    def test_provider_config_required_fields(self):
        """ProviderConfig 必需字段验证"""
        with pytest.raises(Exception):
            ProviderConfig()

        config = ProviderConfig(
            name="test", base_url="https://api.example.com", api_key="key"
        )
        assert config.base_url == "https://api.example.com"
        assert config.api_key == "key"
        assert config.protocol == "openai"

    def test_config_minimal(self, minimal_config_dict):
        """Minimal config builds successfully."""
        config = Config(**minimal_config_dict)
        assert config.providers[0].base_url == "https://api.example.com/v1"
        assert config.providers[0].api_key == "test-key-123"
        assert config.agents[0].model == "gpt-4"

    def test_config_full(self, full_config_dict):
        """Full config builds successfully."""
        config = Config(**full_config_dict)
        assert config.providers[0].base_url == "https://api.example.com/v1"
        assert config.agents[0].model == "gpt-4"
        assert config.agents[0].tools == ["Bash", "Read", "Write"]
        # agents[].provider 已删除：配置里写了也不报错，但解析产物上没有这个字段
        assert not hasattr(config.agents[0], "provider")

    def test_duplicate_provider_name_rejected(self):
        """重复 provider name 报错"""
        with pytest.raises(ValueError, match="duplicate provider name"):
            Config(
                providers=[  # ty: ignore[invalid-argument-type]
                    {"name": "x", "base_url": "http://a", "api_key": "k1"},
                    {"name": "x", "base_url": "http://b", "api_key": "k2"},
                ],
                agents=[{"name": "default", "model": "m"}],  # ty: ignore[invalid-argument-type]
            )

    def test_agent_provider_field_is_silently_ignored(self):
        """`agents[].provider` 已删除：继续写不报错、不生效（extra=ignore 契约）。"""
        config = Config(
            providers=[  # ty: ignore[invalid-argument-type]
                {"name": "a", "base_url": "http://a", "api_key": "k", "models": ["m"]},
            ],
            agents=[  # ty: ignore[invalid-argument-type]
                {"name": "default", "model": "m", "provider": "nonexistent"},
            ],
        )
        assert config.agents[0].model == "m"
        assert not hasattr(config.agents[0], "provider")

    def test_get_provider_helper(self, minimal_config_dict):
        """get_provider 按名称查询（name 必填——没有「默认 provider」概念）"""
        config = Config(**minimal_config_dict)
        p = config.get_provider("default")
        assert p.name == "default"
        with pytest.raises(ValueError, match="provider name is required"):
            config.get_provider("")
        with pytest.raises(ValueError, match="provider 'nope' not found"):
            config.get_provider("nope")


class TestLoadConfig:
    """测试配置加载"""

    def test_get_config_path(self):
        """Config file path is correct."""
        path = get_config_path()
        assert str(path).endswith(".wing/core/config.yaml")
        assert str(path).startswith(str(Path.home()))

    def test_get_wing_home_default(self):
        """Without WING_HOME, returns ~/.wing/core."""
        with mock.patch.dict(os.environ, {}, clear=True):
            home = get_wing_home()
            assert home == Path.home() / ".wing" / "core"

    def test_get_wing_home_env_var(self):
        """With WING_HOME set, returns $WING_HOME/core."""
        with mock.patch.dict(os.environ, {"WING_HOME": "/custom/wing"}, clear=True):
            home = get_wing_home()
            assert home == Path("/custom/wing/core")

    def test_get_wing_home_expands_tilde(self):
        """Tilde in WING_HOME is expanded."""
        with mock.patch.dict(os.environ, {"WING_HOME": "~/my-wing"}, clear=True):
            home = get_wing_home()
            assert home == Path.home() / "my-wing" / "core"

    def test_load_config_file_not_found(self):
        """配置文件不存在时创建模板并抛出错误"""
        with tempfile.TemporaryDirectory() as tmpdir:
            config_path = Path(tmpdir) / ".wing" / "config.yaml"
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=config_path
            ):
                with pytest.raises(RuntimeError) as exc_info:
                    load_config()
                assert "created template" in str(exc_info.value)
                assert config_path.exists()
                content = config_path.read_text()
                assert "providers:" in content
                assert "agents:" in content

    def test_load_config_success(self, minimal_config_dict):
        """成功加载配置"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(minimal_config_dict, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                config = load_config()
                assert config.providers[0].base_url == "https://api.example.com/v1"
                assert config.providers[0].api_key == "test-key-123"
        finally:
            os.unlink(temp_path)

    def test_load_config_full(self, full_config_dict):
        """加载完整配置"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(full_config_dict, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                config = load_config()
                assert config.agents[0].model == "gpt-4"
        finally:
            os.unlink(temp_path)

    def test_load_config_invalid_yaml(self):
        """无效 YAML 文件"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            f.write("invalid: yaml: content: [")
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                with pytest.raises(Exception):
                    load_config()
        finally:
            os.unlink(temp_path)

    def test_load_config_empty_file(self):
        """空配置文件"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            f.write("")
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                with pytest.raises(ValueError) as exc_info:
                    load_config()
                assert "empty" in str(exc_info.value)
        finally:
            os.unlink(temp_path)

    def test_load_config_missing_providers(self):
        """缺少 providers 字段"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump({"agents": [{"name": "x", "model": "gpt-4"}]}, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                with pytest.raises(ValueError) as exc_info:
                    load_config()
                assert "Invalid config" in str(exc_info.value)
        finally:
            os.unlink(temp_path)

    def test_load_config_singleton(self, minimal_config_dict):
        """单例模式测试"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(minimal_config_dict, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                config1 = load_config()
                config2 = load_config()
                assert config1 is config2
        finally:
            os.unlink(temp_path)

    def test_load_config_reload(self, minimal_config_dict):
        """强制重新加载"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(minimal_config_dict, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                config1 = load_config()

                # 修改文件
                new_config = {
                    "providers": [
                        {
                            "name": "default",
                            "base_url": "https://new.example.com",
                            "api_key": "test-key-123",
                            "models": ["gpt-4"],
                        },
                    ],
                    "agents": [{"name": "default", "model": "gpt-4"}],
                }
                with open(temp_path, "w") as f:
                    yaml.dump(new_config, f)

                # 不强制重载，应返回缓存
                config2 = load_config()
                assert config2.providers[0].base_url == "https://api.example.com/v1"

                # 强制重载
                config3 = load_config(reload=True)
                assert config3.providers[0].base_url == "https://new.example.com"
                assert config3 is not config1
        finally:
            os.unlink(temp_path)


class TestGetConfig:
    """测试 get_config 便捷函数"""

    def test_get_config_auto_load(self, minimal_config_dict):
        """get_config 自动加载配置"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(minimal_config_dict, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                config = get_config()
                assert config.providers[0].base_url == "https://api.example.com/v1"
        finally:
            os.unlink(temp_path)

    def test_get_config_caching(self, minimal_config_dict):
        """get_config 缓存测试"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(minimal_config_dict, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                config1 = get_config()
                config2 = get_config()
                assert config1 is config2
        finally:
            os.unlink(temp_path)


class TestResetConfig:
    """测试配置重置"""

    def test_reset_config(self, minimal_config_dict):
        """reset_config 清除缓存"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump(minimal_config_dict, f)
            temp_path = f.name

        try:
            with mock.patch(
                "wing.config.loader.get_config_path", return_value=Path(temp_path)
            ):
                _ = get_config()
                reset_config()
                import wing.config.loader as config_module

                assert config_module._config is None
        finally:
            os.unlink(temp_path)


class TestDefaultConfigTemplate:
    """首启模板（由声明生成）的守门：模板必须是「两个空列表 + 全套注释」。

    这份模板不再是一份能直接跑的配置（那是旧的手写模板 + ``ChangeHere`` 的做法）：
    解析出来只有 ``providers: []`` / ``agents: []``，它们是**天然的 problem**，网关据此进
    setup mode 指路。旧版这一节守的是「手写模板与 models.py 的 SYNC」——模板由声明生成后
    那份纪律死亡，守门改为：① 模板能被解析且恰好是两个空列表；② 加载期只报这两个空列表；
    ③ 每个被展开的字段都以键行或注释行的形式出现（emitter 的覆盖性，见 test_config_emit.py）。
    """

    def test_template_is_two_empty_lists(self):
        from wing.config import build_catalog, default_document, emit_config_yaml

        text = emit_config_yaml(default_document(), build_catalog())
        raw = yaml.safe_load(text)
        assert raw == {"providers": [], "agents": []}
        # 首启模板不再有假值占位符（ChangeHere 死亡）
        assert "ChangeHere" not in text

    def test_template_only_fails_on_empty_providers_and_agents(self):
        """喂给加载期：只失败在「providers / agents 不得为空」这两条上。"""
        from wing.config import (
            build_catalog,
            cross_field_problems,
            default_document,
            emit_config_yaml,
        )

        raw = yaml.safe_load(emit_config_yaml(default_document(), build_catalog()))
        with pytest.raises(ValueError, match="agents list cannot be empty"):
            Config(**raw)
        problems = cross_field_problems(Config.model_construct(**raw))
        assert [(problem.path, problem.kind.value) for problem in problems] == [
            ("agents", "empty_list"),
            ("providers", "empty_list"),
        ]
