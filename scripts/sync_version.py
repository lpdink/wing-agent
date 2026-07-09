#!/usr/bin/env python3
"""Inject version from git tag into Cargo.toml and pin meta-package deps.

Usage:
    python scripts/sync_version.py <version>

This script:
1. Replaces the placeholder version in root Cargo.toml [workspace.package]
2. Pins wing-agent's dependencies to exact versions (wing-cli==X, wing-gateway==X)

Only uses Python standard library.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 2:
        print("Usage: python scripts/sync_version.py <version>", file=sys.stderr)
        sys.exit(1)

    version = sys.argv[1]
    root = Path(__file__).resolve().parent.parent

    # 1. Cargo.toml: replace [workspace.package] version
    cargo_path = root / "Cargo.toml"
    cargo_text = cargo_path.read_text()
    cargo_text = re.sub(
        r"^(\[workspace\.package\]\nversion\s*=\s*)\"[^\"]*\"",
        rf'\1"{version}"',
        cargo_text,
        count=1,
        flags=re.MULTILINE,
    )
    cargo_path.write_text(cargo_text)

    # 2. pyproject.toml: pin dependencies to exact versions
    pyproject_path = root / "pyproject.toml"
    pyproject_text = pyproject_path.read_text()
    pyproject_text = re.sub(
        r'"wing-cli[^"]*"', f'"wing-cli=={version}"', pyproject_text
    )
    pyproject_text = re.sub(
        r'"wing-gateway[^"]*"', f'"wing-gateway=={version}"', pyproject_text
    )
    pyproject_path.write_text(pyproject_text)

    print(f"✅ {version} → Cargo.toml [workspace.package] + pyproject.toml deps")


if __name__ == "__main__":
    main()
