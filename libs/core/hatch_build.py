"""Hatch build hook that bundles the pre-compiled Rust ``wing`` binary into the wheel.

The hook:
1. Compiles the Rust binary via ``cargo build --release``.
2. Uses ``force_include`` to place the binary at ``wing/bin/wing`` inside the wheel.
3. Sets ``infer_tag=True`` so the wheel gets a platform-specific tag
   (e.g. ``cp312-cp312-macosx_11_0_arm64``) instead of ``py3-none-any``.
"""

from __future__ import annotations

import os
import subprocess
import sys
from typing import Any

from hatchling.builders.hooks.plugin.interface import BuildHookInterface  # ty: ignore[unresolved-import]


class WingBinaryHook(BuildHookInterface):
    """Injects the Rust ``wing`` TUI binary into the Python wheel."""

    def initialize(self, _version: str, build_data: dict[str, Any]) -> None:
        # Locate the Cargo workspace root (two levels up from libs/core/).
        workspace_root = os.path.abspath(os.path.join(self.root, "..", ".."))
        cargo_toml = os.path.join(workspace_root, "Cargo.toml")

        if not os.path.isfile(cargo_toml):
            raise FileNotFoundError(
                f"Cargo.toml not found at {cargo_toml}. "
                "Ensure the Rust workspace is intact before building the wheel."
            )

        self.app.display_info("🦀 Compiling wing binary (cargo build --release)...")

        subprocess.run(
            ["cargo", "build", "--release", "--manifest-path", cargo_toml],
            check=True,
            cwd=workspace_root,
        )

        binary_name = "wing.exe" if sys.platform == "win32" else "wing"
        binary_path = os.path.join(workspace_root, "target", "release", binary_name)

        if not os.path.isfile(binary_path):
            raise FileNotFoundError(f"Compiled binary not found at {binary_path}")

        self.app.display_info(f"✅ Binary ready: {binary_path}")

        # Force-include the binary at wing/bin/wing inside the wheel.
        build_data["force_include"][binary_path] = os.path.join(
            "wing", "bin", binary_name
        )

        # Produce a platform-specific wheel tag (e.g. cp312-cp312-macosx_11_0_arm64).
        build_data["infer_tag"] = True
        build_data["pure_python"] = False

    def finalize(
        self, _version: str, _build_data: dict[str, Any], artifact_path: str
    ) -> None:
        self.app.display_info(f"🎉 Wheel built: {os.path.basename(artifact_path)}")

    def clean(self, _versions: list[str]) -> None:
        """Remove build artefacts from the source tree."""
        bin_dir = os.path.join(self.root, "wing", "bin")
        for name in ("wing", "wing.exe"):
            path = os.path.join(bin_dir, name)
            if os.path.isfile(path):
                os.remove(path)
                self.app.display_info(f"🧹 Removed {path}")
