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


def pep440_to_semver(version: str) -> str:
    """Convert PEP 440 version to Cargo-compatible semver.

    Examples:
        0.2.0      → 0.2.0
        0.2.0a1    → 0.2.0-alpha.1
        0.2.0b2    → 0.2.0-beta.2
        0.2.0rc1   → 0.2.0-rc.1
        0.2.0.dev3 → 0.2.0-dev.3
    """
    # Pre-release: a→alpha, b→beta, rc→rc
    v = re.sub(r"(\d)a(\d+)", r"\1-alpha.\2", version)
    v = re.sub(r"(\d)b(\d+)", r"\1-beta.\2", v)
    v = re.sub(r"(\d)rc(\d+)", r"\1-rc.\2", v)
    # Dev release
    v = re.sub(r"\.dev(\d+)", r"-dev.\1", v)
    # Strip local version (+gabc1234) — Cargo doesn't support it
    v = re.sub(r"\+.*$", "", v)
    return v


def main() -> None:
    if len(sys.argv) != 2:
        print("Usage: python scripts/sync_version.py <version>", file=sys.stderr)
        sys.exit(1)

    version = sys.argv[1]
    cargo_version = pep440_to_semver(version)
    root = Path(__file__).resolve().parent.parent

    # 1. Cargo.toml: replace [workspace.package] version (semver format)
    cargo_path = root / "Cargo.toml"
    cargo_text = cargo_path.read_text()
    cargo_text = re.sub(
        r"^(\[workspace\.package\]\nversion\s*=\s*)\"[^\"]*\"",
        rf'\1"{cargo_version}"',
        cargo_text,
        count=1,
        flags=re.MULTILINE,
    )
    cargo_path.write_text(cargo_text)

    # 2. pyproject.toml: pin dependencies to exact versions (PEP 440)
    pyproject_path = root / "pyproject.toml"
    pyproject_text = pyproject_path.read_text()
    pyproject_text = re.sub(
        r'"wing-cli[^"]*"', f'"wing-cli=={version}"', pyproject_text
    )
    pyproject_text = re.sub(
        r'"wing-gateway[^"]*"', f'"wing-gateway=={version}"', pyproject_text
    )
    pyproject_path.write_text(pyproject_text)

    if cargo_version != version:
        print(f"✅ PEP440 {version} → Cargo {cargo_version} + PyPI deps pinned")
    else:
        print(f"✅ {version} → Cargo.toml + pyproject.toml deps")


if __name__ == "__main__":
    main()
