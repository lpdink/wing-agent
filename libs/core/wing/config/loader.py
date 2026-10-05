# wing/config/loader.py
"""Unified configuration module.

Loads from ``$WING_HOME/core/config.yaml`` (default: ``~/.wing/core/config.yaml``).
When the config file is missing, a template is created from ``default_config.py``.
"""

import os
from pathlib import Path
from typing import Optional

import yaml

from .models import Config


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
    from .default_config import DEFAULT_CONFIG_YAML

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
