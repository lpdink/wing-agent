# wing/config/__init__.py
"""wing/config 包 — 配置模型、加载单例与默认模板。

公共 API 通过此 __init__ re-export（消费方 import 路径不变）：

    from wing.config import Config, get_config, load_config, get_headers, ...

内部模块：``models``（pydantic 配置模型 + 模型能力 / 展示名解析）·
``loader``（WING_HOME 解析 + 配置单例加载 + load_hooks——待 11 归位 hooks/）·
``user_agent``（UA 预设与请求头构造）·
``default_config``（手写默认 config.yaml 模板，事实来源）。
"""

from .default_config import DEFAULT_CONFIG_YAML
from .loader import (
    get_config,
    get_config_path,
    get_wing_home,
    load_config,
    load_hooks,
    reset_config,
)
from .models import (
    AgentConfig,
    ApiKeyEntry,
    AuthConfig,
    CommandsConfig,
    Config,
    EvictionConfig,
    GatewayConfig,
    ImagesConfig,
    LogConfig,
    ModelCapabilities,
    ModelSpec,
    ProviderConfig,
    SessionsConfig,
    ToolResultTruncateConfig,
    UserAgentConfig,
    resolve_model_capabilities,
    resolve_model_display_name,
)
from .user_agent import get_headers

__all__ = [
    "AgentConfig",
    "ApiKeyEntry",
    "AuthConfig",
    "CommandsConfig",
    "Config",
    "DEFAULT_CONFIG_YAML",
    "EvictionConfig",
    "GatewayConfig",
    "ImagesConfig",
    "LogConfig",
    "ModelCapabilities",
    "ModelSpec",
    "ProviderConfig",
    "SessionsConfig",
    "ToolResultTruncateConfig",
    "UserAgentConfig",
    "get_config",
    "get_config_path",
    "get_headers",
    "get_wing_home",
    "load_config",
    "load_hooks",
    "reset_config",
    "resolve_model_capabilities",
    "resolve_model_display_name",
]
