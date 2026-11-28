#!/usr/bin/env python3
"""发布门禁：wheel 必须自报本次构建的 commit。

两类产物、两条注入路径，各自验一件事：

* ``wing_cli-*.whl`` —— 二进制 ``--version`` 必须带 ``(<HEAD 的 7 位>)``。commit 由
  ``crates/wing/build.rs`` 解析（env → git → ``crates/wing/build_commit``），manylinux
  的 wheel 在 Docker 里构建，只有文件那一档可用；只挡 ``unknown`` 不够 —— 一个陈旧或
  错写的 commit 同样会静默进 PyPI（见 ``docs/dev/architecture.md``「构建信息注入」）。
* ``wing_gateway-*.whl`` —— ``wing/_build_info.py`` 的 ``commit`` 必须非 None（hatch
  钩子走 env → git，不经容器）。

这条不变量只能在发布流水线验证：CI 不构建 wheel（manylinux 需要容器，成本太高），
所以 PR 期由 ``build_meta.rs`` 的单测守三档顺序，发布期由本脚本守真实产物。

用法：``assert_wheel_commit.py dist/``（目录里可以同时有多个 wheel）
退出码：0 = 全部通过；1 = 任一失败（原因打到 stderr）。
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

#: `wing 0.7.0 (a7e7126) built ... [target]` —— 长 `--version` 才带 commit（`-V` 不带）。
VERSION_LINE = re.compile(r"^wing (\S+) \(([^)]+)\)")
#: hatch 生成文件里的一行：`commit: str | None = 'a7e7126'`（也可能是 `= None`）
BUILD_INFO_COMMIT = re.compile(r"^commit: str \| None = (.+)$", re.MULTILINE)
#: 开发构建的占位版本（`scripts/sync_version.py` 没生效时会原样发出去）。
PLACEHOLDER_VERSION = "0.0.0"


def fail(message: str) -> None:
    print(f"::error::{message}", file=sys.stderr)
    raise SystemExit(1)


def expected_commit() -> str:
    """本次构建的 HEAD（与写入 `build_commit` 的同一个事实来源）。

    不用 ``$GITHUB_SHA``：annotated tag 推送时它未必等于 checkout 出来的 HEAD。
    """
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        fail(f"git rev-parse HEAD failed: {result.stderr.strip()}")
    return result.stdout.strip()[:7]


def check_cli(wheel: Path, want: str) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        with zipfile.ZipFile(wheel) as archive:
            members = [
                n for n in archive.namelist() if n.endswith(".data/scripts/wing")
            ]
        if len(members) != 1:
            fail(f"{wheel.name}: expected one .data/scripts/wing, got {members}")

        result = subprocess.run(
            ["unzip", "-o", "-q", str(wheel), members[0], "-d", tmp],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            fail(f"{wheel.name}: unzip failed: {result.stderr.strip()}")

        binary = Path(tmp) / members[0]
        binary.chmod(0o755)
        version = subprocess.run(
            [str(binary), "--version"], capture_output=True, text=True, check=False
        )
        if version.returncode != 0:
            fail(f"{wheel.name}: --version exited {version.returncode}")
        line = version.stdout.strip()
        print(f"{wheel.name}: {line}")

        match = VERSION_LINE.match(line)
        if match is None:
            fail(f"{wheel.name}: unparsable --version line: {line!r}")
        version_field, commit_field = match.groups()
        if version_field == PLACEHOLDER_VERSION:
            fail(f"{wheel.name}: version not injected ({version_field})")
        if commit_field != want:
            fail(f"{wheel.name}: commit is {commit_field!r}, expected {want!r}")


def check_gateway(wheel: Path) -> None:
    with zipfile.ZipFile(wheel) as archive:
        try:
            build_info = archive.read("wing/_build_info.py").decode()
        except KeyError:
            fail(f"{wheel.name}: wing/_build_info.py missing")

    match = BUILD_INFO_COMMIT.search(build_info)
    if match is None:
        fail(f"{wheel.name}: no commit field in _build_info.py")
    raw = match.group(1).strip()
    if raw in {"None", "''", '""'}:
        fail(f"{wheel.name}: commit not injected ({raw})")
    commit = raw.strip("'\"")
    print(f"{wheel.name}: commit={commit}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dist", type=Path, help="directory holding the built wheels")
    args = parser.parse_args()

    wheels = sorted(args.dist.glob("*.whl"))
    if not wheels:
        fail(f"{args.dist}: no wheels")

    want = expected_commit()
    print(f"expected commit: {want}")
    checked = 0
    for wheel in wheels:
        if wheel.name.startswith("wing_cli-"):
            check_cli(wheel, want)
            checked += 1
        elif wheel.name.startswith("wing_gateway-"):
            check_gateway(wheel)
            checked += 1
        else:
            print(f"{wheel.name}: no commit contract, skipped")
    if checked == 0:
        fail(f"{args.dist}: no wing_cli / wing_gateway wheel to check")
    print(f"✅ {checked} wheel(s) report their build commit")
    return 0


if __name__ == "__main__":
    sys.exit(main())
