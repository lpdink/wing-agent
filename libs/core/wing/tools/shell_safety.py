# wing/tools/shell_safety.py
"""Shell command safety checker.

Simple two-layer model:

    1. Config whitelist (safe_command_patterns) → allow
    2. Default                                   → block (prompt)

The previous safe-read allowlist has been removed due to security
gaps. Users who want to auto-approve specific commands should add
regex patterns to ``safe_command_patterns`` in config.yaml, or
enable ``yolo: true`` to skip all checks.
"""

from __future__ import annotations

import re


def is_dangerous_command(command: str | None) -> bool:
    """Determine if a Bash command requires user approval.

    Two-layer model:
        1. Config whitelist (safe_command_patterns) → allow (return False)
        2. Default                                   → block (return True)

    Args:
        command: The command string to check.

    Returns:
        True if the command is dangerous (needs approval),
        False if it matches the whitelist (can be auto-executed).
    """
    if not isinstance(command, str) or not command.strip():
        return False

    cmd = command.strip()

    # Config whitelist (safe_command_patterns)
    from wing.config import get_config

    patterns = get_config().safe_command_patterns
    for pattern in patterns:
        if re.search(pattern, cmd):
            return False

    return True
