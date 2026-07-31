"""Tests for is_dangerous_command (whitelist + default-block model)."""

from unittest.mock import patch

from wing.config import AgentConfig, Config, ProviderConfig
from wing.tools.shell_safety import is_dangerous_command


def _cfg(safe_patterns: list[str] | None = None) -> Config:
    """Create a Config with optional safe_command_patterns."""
    return Config(
        providers=[
            ProviderConfig(
                name="default", base_url="https://api.example.com", api_key="test"
            )
        ],
        agents=[AgentConfig(name="default", model="gpt-4")],
        safe_command_patterns=safe_patterns or [],
    )


def _patch_cfg(safe_patterns: list[str] | None = None):
    """Patch get_config with a test Config."""
    return patch("wing.config.get_config", return_value=_cfg(safe_patterns))


# ---------------------------------------------------------------------------
# Edge cases
# ---------------------------------------------------------------------------


class TestEdgeCases:
    def test_empty_string(self):
        with _patch_cfg():
            assert is_dangerous_command("") is False

    def test_none_input(self):
        with _patch_cfg():
            assert is_dangerous_command(None) is False

    def test_whitespace_only(self):
        with _patch_cfg():
            assert is_dangerous_command("   ") is False


# ---------------------------------------------------------------------------
# Default: all non-empty commands are dangerous
# ---------------------------------------------------------------------------


class TestDefaultBlock:
    """Without whitelist, all commands are dangerous."""

    def test_ls_is_dangerous(self):
        with _patch_cfg():
            assert is_dangerous_command("ls -la") is True

    def test_cat_is_dangerous(self):
        with _patch_cfg():
            assert is_dangerous_command("cat file.txt") is True

    def test_git_status_is_dangerous(self):
        with _patch_cfg():
            assert is_dangerous_command("git status") is True

    def test_rm_is_dangerous(self):
        with _patch_cfg():
            assert is_dangerous_command("rm -rf /tmp/test") is True


# ---------------------------------------------------------------------------
# Whitelist: matching commands are safe
# ---------------------------------------------------------------------------


class TestWhitelist:
    """safe_command_patterns whitelist allows matching commands."""

    def test_exact_pattern(self):
        with _patch_cfg(["^ls"]):
            assert is_dangerous_command("ls -la") is False

    def test_regex_pattern(self):
        with _patch_cfg([r"^git\s+(status|log|diff)"]):
            assert is_dangerous_command("git status") is False
            assert is_dangerous_command("git log --oneline") is False
            assert is_dangerous_command("git diff HEAD") is False

    def test_non_matching_is_dangerous(self):
        with _patch_cfg([r"^git\s+(status|log|diff)"]):
            assert is_dangerous_command("git push") is True
            assert is_dangerous_command("rm file.txt") is True

    def test_multiple_patterns(self):
        with _patch_cfg([r"^ls\b", r"^cat\b", r"^git\s+status"]):
            assert is_dangerous_command("ls -la") is False
            assert is_dangerous_command("cat file.txt") is False
            assert is_dangerous_command("git status") is False
            assert is_dangerous_command("rm file.txt") is True

    def test_broad_pattern(self):
        with _patch_cfg([".*"]):
            assert is_dangerous_command("anything") is False
