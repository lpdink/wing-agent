# wing/config/__init__.py
"""wing/config 包 — 配置模型、加载单例、设置目录树与规范形 YAML emitter。

公共 API 通过此 __init__ re-export（消费方 import 路径不变）：

    from wing.config import Config, get_config, load_config, get_headers, S, build_catalog, ...

内部模块：``models``（pydantic 配置模型 + 模型目录（id 空间）/ 能力 / 展示名解析）·
``spec``（声明层：``S(...)`` / ``SettingMeta`` / ``ApplyScope``，字段元信息的唯一来源）·
``problems``（跨字段检查的纯函数 + ``ConfigProblem``，加载期与设置面板共用）·
``catalog``（设置目录树：``SettingNode`` / ``build_catalog()`` / ``parse_path()``）·
``emit``（规范形 YAML emitter：默认模板与保存路径共用，注释来自声明）·
``loader``（WING_HOME 解析 + 配置单例加载）·
``user_agent``（UA 预设与请求头构造）。
"""

from .catalog import (
    ChoiceSpec,
    PathStep,
    SettingKind,
    SettingNode,
    build_catalog,
    parse_path,
)
from .emit import default_document, emit_config_yaml
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
    "ChoiceSpec",
    "CommandsConfig",
    "Config",
    "ConfigProblem",
    "EvictionConfig",
    "GatewayConfig",
    "ImagesConfig",
    "LogConfig",
    "ModelCapabilities",
    "ModelGroup",
    "ModelRef",
    "ModelSpec",
    "PathStep",
    "ProblemKind",
    "ProviderConfig",
    "S",
    "SessionsConfig",
    "SettingKind",
    "SettingMeta",
    "SettingNode",
    "ToolResultTruncateConfig",
    "UserAgentConfig",
    "build_catalog",
    "cross_field_problems",
    "default_document",
    "emit_config_yaml",
    "get_config",
    "get_config_path",
    "get_headers",
    "get_wing_home",
    "load_config",
    "parse_path",
    "reset_config",
    "resolve_model_capabilities",
    "resolve_model_display_name",
    "setting_meta",
]
