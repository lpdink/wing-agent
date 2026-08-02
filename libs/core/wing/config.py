# wing/config.py
"""Unified configuration module.

Loads from ``$WING_HOME/core/config.yaml`` (default: ``~/.wing/core/config.yaml``).
When the config file is missing, a template is created from ``default_config.py``.

# SYNC: Keep this file in sync with default_config.py when adding/removing fields.
"""

import hmac
import os
from pathlib import Path
from typing import Literal, Optional

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


class ProviderConfig(BaseModel):
    """单个 LLM provider 配置。"""

    model_config = ConfigDict(extra="ignore")

    name: str
    """用户自定义标识，全局唯一。"""
    protocol: Literal["openai", "anthropic"] = "openai"
    """协议类型。"""
    base_url: str
    api_key: str
    timeout_first_chunk: float = 300.0
    timeout_total: float = 600.0
    explicit_cache_mode: bool = True
    reasoning_effort: str | None = None
    max_tokens: int = 128_000
    """最大输出 token 数（Anthropic 协议必填）。"""
    extra_body: dict = Field(default_factory=dict)
    """透传到 request body 的额外字段（平铺合并到顶层）。"""
    anthropic_version: str = "2023-06-01"
    """Anthropic API 版本 header（仅 anthropic 协议使用）。"""
    models: list[str] = Field(default_factory=list)
    """静态模型列表。配置后不再请求远端 GET /models。"""

    @field_validator("name")
    @classmethod
    def _name_valid(cls, v: str) -> str:
        import re

        if not re.match(r"^[a-zA-Z0-9_-]+$", v):
            raise ValueError(f"provider name must match ^[a-zA-Z0-9_-]+$, got: '{v}'")
        return v


class UserAgentConfig(BaseModel):
    preset: Literal["opencode", "qwen-code"] = "qwen-code"


class LogConfig(BaseModel):
    level: str = "WARNING"


class SessionsConfig(BaseModel):
    model_config = ConfigDict(extra="ignore")

    def resolved_path(self) -> Path:
        """Session storage path.

        Priority: WING_SESSIONS_PATH env > get_wing_home() / "sessions".
        """
        env_path = os.environ.get("WING_SESSIONS_PATH")
        if env_path:
            return Path(env_path).expanduser()
        return get_wing_home() / "sessions"


class AgentConfig(BaseModel):
    model_config = ConfigDict(extra="ignore")

    name: str
    model: str
    provider: str | None = None
    """引用的 providers[].name。None 时绑定第一个 provider。"""
    default: bool = False
    system_prompt: str = ""
    tools: list[str] = Field(default_factory=list)
    context_window_tokens: int = 256_000
    keep_recent_tokens: int = 50_000
    skills: list[str] = Field(default_factory=list)
    rules: list[str] = Field(default_factory=list)
    max_turns: int | None = None
    yolo: bool | None = None


class ToolResultTruncateConfig(BaseModel):
    """Tool result truncation policy.

    max_length: trigger threshold in chars. None or <0 disables truncation.
    keep_chars: number of chars to keep at head and tail when truncating.
    """

    max_length: int | None = 100_000
    keep_chars: int = 200

    @field_validator("keep_chars")
    @classmethod
    def _keep_chars_non_negative(cls, v: int) -> int:
        if v < 0:
            raise ValueError("keep_chars must be >= 0")
        return v


class ApiKeyEntry(BaseModel):
    """Single API key with an identity role.

    Keys are restricted to ASCII printable characters (0x20–0x7E).
    HTTP headers are latin-1 encoded; non-ASCII keys would silently
    mismatch between client and server.
    """

    key: str
    role: str = "admin"

    @field_validator("key")
    @classmethod
    def _key_ascii_printable(cls, v: str) -> str:
        if not v or not all(0x20 <= ord(c) <= 0x7E for c in v):
            raise ValueError(
                "API key must contain only ASCII printable characters (0x20-0x7E)"
            )
        return v


class AuthConfig(BaseModel):
    """Gateway API key authentication configuration.

    When ``enabled`` is True, all HTTP/WS requests (except exempt paths)
    must carry a valid API key.  ``role`` is enforced (RBAC): ``admin``
    has full access; ``tool_runtime`` may only register remote tools.
    """

    enabled: bool = False
    keys: list[ApiKeyEntry] = Field(default_factory=list)

    def verify(self, key: str) -> str | None:
        """Return the role for *key*, or ``None`` if not found.

        Uses constant-time comparison to prevent timing attacks.
        """
        key_bytes = key.encode("utf-8")
        for entry in self.keys:
            if hmac.compare_digest(key_bytes, entry.key.encode("utf-8")):
                return entry.role
        return None


class GatewayConfig(BaseModel):
    host: str = "127.0.0.1"
    port: int = 32523
    auth: AuthConfig = Field(default_factory=AuthConfig)
    remote_tool_timeout: float = 1800.0
    """远程工具调用总超时（秒）——安全网，非小超时。

    远程工具（如 Bash）可能执行很久，故默认宽口径（30 分钟）。
    断连是首要失败信号（WS 关闭立即 fail 在途调用），此超时仅兜底
    客户端静默挂死但未断连的极端情况。
    """


class CommandsConfig(BaseModel):
    paths: list[str] = Field(default_factory=list)


class Config(BaseModel):
    model_config = ConfigDict(extra="ignore")

    providers: list[ProviderConfig]
    agents: list[AgentConfig]
    hooks: list[str] = Field(default_factory=list)
    safe_command_patterns: list[str] = Field(default_factory=list)
    yolo: bool = False
    steer: bool = True
    preserved_thinking: bool = True
    log: LogConfig = Field(default_factory=LogConfig)
    sessions: SessionsConfig = Field(default_factory=SessionsConfig)
    commands: CommandsConfig = Field(default_factory=CommandsConfig)
    user_agent: UserAgentConfig = Field(default_factory=UserAgentConfig)
    gateway: GatewayConfig = Field(default_factory=GatewayConfig)
    tool_result_truncate: ToolResultTruncateConfig = Field(
        default_factory=ToolResultTruncateConfig
    )

    @model_validator(mode="after")
    def _validate_config(self) -> "Config":
        if not self.agents:
            raise ValueError("agents list cannot be empty")
        if not self.providers:
            raise ValueError("providers list cannot be empty")

        # provider name 唯一性
        provider_names = [p.name for p in self.providers]
        if len(provider_names) != len(set(provider_names)):
            seen = set()
            for n in provider_names:
                if n in seen:
                    raise ValueError(f"duplicate provider name: '{n}'")
                seen.add(n)

        # agent name 唯一性
        agent_names = [a.name for a in self.agents]
        if len(agent_names) != len(set(agent_names)):
            seen_agents: set[str] = set()
            for n in agent_names:
                if n in seen_agents:
                    raise ValueError(f"duplicate agent name: '{n}'")
                seen_agents.add(n)

        # agent.provider 引用存在性；未指定时落定第一个 provider（解析阶段
        # 消除可选性——解析产物 AgentTemplate 的 provider_name 为必填）
        provider_name_set = set(provider_names)
        for agent in self.agents:
            if agent.provider is None:
                agent.provider = provider_names[0]
            elif agent.provider not in provider_name_set:
                raise ValueError(
                    f"agent '{agent.name}' references unknown provider '{agent.provider}'"
                )

        return self

    def get_provider(self, name: str | None = None) -> ProviderConfig:
        """获取 provider 配置。name 为 None 时返回第一个。"""
        if name is None:
            return self.providers[0]
        for p in self.providers:
            if p.name == name:
                return p
        raise ValueError(f"provider '{name}' not found")


_config: Optional[Config] = None


def get_wing_home() -> Path:
    """Wing home directory for backend data.

    Returns ``$WING_HOME/core`` or ``~/.wing/core``.
    """
    env_home = os.environ.get("WING_HOME")
    base = Path(env_home).expanduser() if env_home else Path.home() / ".wing"
    return base / "core"


def get_config_path() -> Path:
    return get_wing_home() / "config.yaml"


def _create_config_template(config_path: Path) -> None:
    """Create config file from the default template."""
    from wing.default_config import DEFAULT_CONFIG_YAML

    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.write_text(DEFAULT_CONFIG_YAML, encoding="utf-8")
    print(f"Created config template: {config_path}")


def load_config(reload: bool = False) -> Config:
    """Load configuration (singleton).

    Args:
        reload: Force reload from disk.

    Raises:
        RuntimeError: Config file was missing (template created).
        ValueError: Config file is malformed or missing required fields.
    """
    global _config

    if _config is not None and not reload:
        return _config

    config_path = get_config_path()
    if not config_path.exists():
        _create_config_template(config_path)
        raise RuntimeError(
            f"Config file not found, created template: {config_path}\n"
            "Please edit the config file and restart."
        )

    with open(config_path, "r", encoding="utf-8") as f:
        data = yaml.safe_load(f)

    if not data:
        raise ValueError(f"Config file is empty: {config_path}")

    try:
        _config = Config(**data)
        print(f"load config from: {config_path}")
        return _config
    except Exception as e:
        raise ValueError(f"Invalid config: {e}") from e


def get_config() -> Config:
    """Get the global config instance (lazy-loads on first call)."""
    if _config is None:
        return load_config()
    return _config


def reset_config() -> None:
    """Reset config cache (mainly for tests)."""
    global _config
    _config = None


# ── User-Agent constants ─────────────────────────────────────

_OPENCODE_VERSION = "1.3.17"
_QWEN_CODE_VERSION = "0.14.3"


def _get_platform_info() -> tuple[str, str]:
    import platform

    return platform.system().lower(), platform.machine().lower()


def _build_opencode_ua(system: str, arch: str) -> str:
    import platform

    release = platform.release()
    return f"opencode/{_OPENCODE_VERSION} ({system} {release}; {arch})"


def _build_qwen_code_ua(system: str, arch: str) -> str:
    return f"QwenCode/{_QWEN_CODE_VERSION} ({system}; {arch})"


def get_headers() -> dict[str, str]:
    config = get_config()
    preset = config.user_agent.preset
    system, arch = _get_platform_info()

    if preset == "opencode":
        ua = _build_opencode_ua(system, arch)
    elif preset == "qwen-code":
        ua = _build_qwen_code_ua(system, arch)
    else:
        raise ValueError(f"Unknown user_agent preset: {preset}")

    if preset == "qwen-code":
        return {
            "User-Agent": ua,
            "X-DashScope-CacheControl": "enable",
            "X-DashScope-UserAgent": ua,
            "X-DashScope-AuthType": "qwen-oauth",
        }

    return {"User-Agent": ua}


def load_hooks(hooks_patterns: list[str]) -> None:
    """Load hook files from glob patterns.

    Each matched .py file is imported; hook registration happens
    via the ``hooks.on()`` decorator during import.
    """
    import glob
    import importlib
    import importlib.util

    from wing.common.logger import log

    for pattern in hooks_patterns:
        expanded = Path(pattern).expanduser()
        matched_files = sorted(glob.glob(str(expanded)))
        if not matched_files:
            log.info(f"No hook files matched pattern: {pattern}")
            continue

        for file_path in matched_files:
            file_path = Path(file_path)
            if not file_path.is_file() or file_path.suffix != ".py":
                continue

            module_name = f"wing_hook_{file_path.stem}"

            try:
                spec = importlib.util.spec_from_file_location(
                    module_name, str(file_path)
                )
                if spec is None or spec.loader is None:
                    log.warning(f"Cannot create import spec for hook file: {file_path}")
                    continue
                module = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(module)
                log.info(f"Loaded hook file: {file_path}")
            except Exception as e:
                log.warning(f"Failed to load hook file {file_path}: {e}")
