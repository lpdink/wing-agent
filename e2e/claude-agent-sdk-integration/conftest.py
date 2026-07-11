"""Pytest configuration for e2e tests.

E2E tests verify wing's stdio mode works correctly when driven by the
``claude-agent-sdk-python`` SDK as the external orchestration layer.

Prerequisites:
    - ``wing`` binary built: ``cargo build --release``
    - Gateway auto-starts via wing's ``ensure_gateway_running()``
    - LLM API key configured in ``~/.wing/core/config.yaml``
"""

import os
import shutil
from pathlib import Path
from typing import Callable

import pytest

from claude_agent_sdk import ClaudeAgentOptions

# Skip the SDK's ``wing -v`` version check — wing is not Claude Code,
# so the version string won't match the Claude minimum version regex.
os.environ["CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK"] = "1"

# Route session storage to /tmp so e2e runs don't pollute ~/.wing/sessions.
os.environ.setdefault("WING_SESSIONS_PATH", "/tmp/wing-e2e-sessions")


def _resolve_wing_binary() -> str:
    """Locate the wing binary.

    Priority:
        1. ``$WING_BINARY`` env var (explicit override)
        2. ``target/release/wing`` in the project root
        3. ``wing`` on ``$PATH`` (e.g. installed via ``cargo install``)
    """
    # 1. Explicit env var.
    env_path = os.environ.get("WING_BINARY")
    if env_path and Path(env_path).is_file():
        return env_path

    # 2. Project root target/release/wing.
    project_root = Path(__file__).resolve().parent.parent.parent
    release_binary = project_root / "target" / "release" / "wing"
    if release_binary.is_file():
        return str(release_binary)

    # 3. PATH lookup.
    which = shutil.which("wing")
    if which:
        return which

    pytest.fail(
        "wing binary not found. Build it first:\n"
        "  cargo build --release\n"
        "Or set WING_BINARY=/path/to/wing"
    )


@pytest.fixture(scope="session")
def wing_binary_path() -> str:
    """Resolve and return the wing binary path (session-scoped)."""
    return _resolve_wing_binary()


@pytest.fixture
def sdk_options_factory(wing_binary_path: str) -> Callable[..., ClaudeAgentOptions]:
    """Factory fixture returning ``ClaudeAgentOptions`` pre-configured for wing.

    Usage::

        def test_foo(sdk_options_factory):
            options = sdk_options_factory(max_turns=3)
    """

    def _factory(**overrides: object) -> ClaudeAgentOptions:
        defaults: dict = {
            "cli_path": wing_binary_path,
            "permission_mode": "bypassPermissions",
            "max_turns": 5,
        }
        defaults.update(overrides)
        return ClaudeAgentOptions(**defaults)

    return _factory


@pytest.fixture
def anyio_backend() -> str:
    """Pin e2e tests to asyncio (no trio — real subprocess calls)."""
    return "asyncio"
