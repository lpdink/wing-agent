# wing/config/loader.py
"""Unified configuration module.

Loads from ``$WING_HOME/core/config.yaml`` (default: ``~/.wing/core/config.yaml``).
When the config file is missing, a template is generated from the declarations
(``config/catalog.py`` + ``config/emit.py``) — one emitter for the default template and for
every save, so the file can never drift from the models again.
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
    """Create config file from the generated template (声明 → catalog → emitter)。

    原子写（tmp + fsync + replace）：首启的模板与保存路径共用同一条落盘语义，
    不留下半截文件。
    """
    from ..common.fs import atomic_write_text
    from .catalog import build_catalog
    from .emit import default_document, emit_config_yaml

    text = emit_config_yaml(default_document(), build_catalog())
    atomic_write_text(config_path, text)
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

    return load_config_text(config_path.read_text(encoding="utf-8"))


def load_config_text(text: str) -> Config:
    """Validate and install the exact content of a saved configuration."""
    global _config
    data = yaml.safe_load(text)
    if not data:
        raise ValueError("config.yaml is empty")
    try:
        _config = Config(**data)
    except Exception as exc:
        raise ValueError(f"Invalid config: {exc}") from exc
    print(f"load config from: {get_config_path()}")
    return _config


def get_config() -> Config:
    """Get the global config instance (lazy-loads on first call)."""
    if _config is None:
        return load_config()
    return _config


def reset_config() -> None:
    """Reset config cache (mainly for tests)."""
    global _config
    _config = None
