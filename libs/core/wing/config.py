# wing/config.py
"""Unified configuration module.

Loads from ``$WING_HOME/core/config.yaml`` (default: ``~/.wing/core/config.yaml``).
When the config file is missing, a template is created from ``default_config.py``.

# SYNC: Keep this file in sync with default_config.py when adding/removing fields.
"""

import os
from pathlib import Path
from typing import Literal, Optional

import yaml
from pydantic import BaseModel, ConfigDict, Field, model_validator


class OpenAIConfig(BaseModel):
    base_url: str
    api_key: str
    timeout_first_chunk: float = 300.0
    timeout_total: float = 600.0
    explicit_cache_mode: bool = True
    reasoning_effort: str | None = None


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
    default: bool = False
    system_prompt: str = ""
    tools: list[str] = Field(default_factory=list)
    context_window_tokens: int = 256_000
    keep_recent_tokens: int = 50_000
    skills: list[str] = Field(default_factory=list)
    rules: list[str] = Field(default_factory=list)


class GatewayConfig(BaseModel):
    host: str = "127.0.0.1"
    port: int = 32523


class CommandsConfig(BaseModel):
    paths: list[str] = Field(default_factory=list)


class Config(BaseModel):
    model_config = ConfigDict(extra="ignore")

    openai: OpenAIConfig
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

    @model_validator(mode="after")
    def _validate_agents(self) -> "Config":
        if not self.agents:
            raise ValueError("agents list cannot be empty")
        names = [a.name for a in self.agents]
        if len(names) != len(set(names)):
            seen = set()
            for n in names:
                if n in seen:
                    raise ValueError(f"duplicate agent name: '{n}'")
                seen.add(n)
        return self


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
