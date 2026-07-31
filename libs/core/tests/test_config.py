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
    """最小配置字典（仅必需字段）"""
    return {
        "providers": [
            {
                "name": "default",
                "base_url": "https://api.example.com/v1",
                "api_key": "test-key-123",
            },
        ],
        "agents": [
            {"name": "default", "model": "gpt-4"},
        ],
    }


@pytest.fixture
def full_config_dict() -> dict:
    """完整配置字典"""
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
            ProviderConfig()  # ty: ignore[missing-argument]

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
        assert config.agents[0].provider == "main"

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

    def test_agent_references_unknown_provider(self):
        """agent 引用不存在的 provider 报错"""
        with pytest.raises(ValueError, match="unknown provider"):
            Config(
                providers=[  # ty: ignore[invalid-argument-type]
                    {"name": "a", "base_url": "http://a", "api_key": "k"},
                ],
                agents=[{"name": "default", "model": "m", "provider": "nonexistent"}],  # ty: ignore[invalid-argument-type]
            )

    def test_get_provider_helper(self, minimal_config_dict):
        """get_provider 按名称查询"""
        config = Config(**minimal_config_dict)
        p = config.get_provider("default")
        assert p.name == "default"
        # None 返回第一个
        p2 = config.get_provider(None)
        assert p2.name == "default"


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
            with mock.patch("wing.config.get_config_path", return_value=config_path):
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
            ):
                config1 = load_config()

                # 修改文件
                new_config = {
                    "providers": [
                        {
                            "name": "default",
                            "base_url": "https://new.example.com",
                            "api_key": "test-key-123",
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
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
                "wing.config.get_config_path", return_value=Path(temp_path)
            ):
                _ = get_config()
                reset_config()
                import wing.config as config_module

                assert config_module._config is None
        finally:
            os.unlink(temp_path)
