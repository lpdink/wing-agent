# wing/config/__init__.py
"""wing/config 包 — 配置模型、加载单例与默认模板。

公共 API 通过此 __init__ re-export（消费方 import 路径不变）：

    from wing.config import Config, get_config, load_config, get_headers, S, ...

内部模块：``models``（pydantic 配置模型 + 模型目录（id 空间）/ 能力 / 展示名解析）·
``spec``（声明层：``S(...)`` / ``SettingMeta`` / ``ApplyScope``，字段元信息的唯一来源）·
``problems``（跨字段检查的纯函数 + ``ConfigProblem``，加载期与设置面板共用）·
``loader``（WING_HOME 解析 + 配置单例加载）·
``user_agent``（UA 预设与请求头构造）·
``default_config``（手写默认 config.yaml 模板，事实来源）。
"""

from .default_config import DEFAULT_CONFIG_YAML
from .loader import (
    get_config,
    get_config_path,
    get_wing_home,
    load_config,
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
    ModelGroup,
    ModelRef,
    ModelSpec,
    ProviderConfig,
    SessionsConfig,
    ToolResultTruncateConfig,
    UserAgentConfig,
    resolve_model_capabilities,
    resolve_model_display_name,
)
from .problems import ConfigProblem, ProblemKind, cross_field_problems
from .spec import ApplyScope, SettingMeta, S, setting_meta
from .user_agent import get_headers

__all__ = [
    "AgentConfig",
    "ApiKeyEntry",
    "ApplyScope",
    "AuthConfig",
    "CommandsConfig",
    "Config",
    "ConfigProblem",
    "DEFAULT_CONFIG_YAML",
    "EvictionConfig",
    "GatewayConfig",
    "ImagesConfig",
    "LogConfig",
    "ModelCapabilities",
    "ModelGroup",
    "ModelRef",
    "ModelSpec",
    "ProblemKind",
    "ProviderConfig",
    "S",
    "SessionsConfig",
    "SettingMeta",
    "ToolResultTruncateConfig",
    "UserAgentConfig",
    "cross_field_problems",
    "get_config",
    "get_config_path",
    "get_headers",
    "get_wing_home",
    "load_config",
    "reset_config",
    "resolve_model_capabilities",
    "resolve_model_display_name",
    "setting_meta",
]
