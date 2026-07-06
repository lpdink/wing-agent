"""Entry point that exec's the bundled Rust ``wing`` binary.

Using ``os.execv`` instead of ``subprocess`` so that:
- The process is replaced entirely (zero overhead).
- TTY, signals, and exit codes pass through transparently —
  critical for the ratatui TUI.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path


def _binary_path() -> Path:
    """Return the path to the bundled ``wing`` binary."""
    name = "wing.exe" if sys.platform == "win32" else "wing"
    return Path(__file__).resolve().parent / name


def main() -> None:
    binary = _binary_path()

    if not binary.is_file():
        print(
            f"Error: wing binary not found at {binary}\n"
            "The wheel may have been built without the Rust binary.",
            file=sys.stderr,
        )
        sys.exit(1)

    # Ensure the binary is executable (matters after pip unpacks the wheel).
    binary.chmod(binary.stat().st_mode | 0o111)

    # Replace the current process with the Rust binary.
    os.execv(str(binary), [str(binary), *sys.argv[1:]])
