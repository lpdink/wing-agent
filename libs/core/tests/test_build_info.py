"""构建信息注入测试。

两层覆盖：``libs/core/hatch_build.py``（构建/安装时解析 commit、渲染生成
文件）与 ``wing.build_info``（运行期读取生成文件的唯一入口）。
"""

from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import sys
from pathlib import Path
from types import ModuleType
from unittest.mock import MagicMock

import pytest

from wing import build_info

_PACKAGE_ROOT = Path(__file__).resolve().parents[1]
_REPO_ROOT = _PACKAGE_ROOT.parents[1]


def _load_hatch_build() -> ModuleType:
    """按路径加载构建钩子——hatch_build.py 在包外，只被构建后端加载。"""
    path = _PACKAGE_ROOT / "hatch_build.py"
    spec = importlib.util.spec_from_file_location("hatch_build", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


hatch_build = _load_hatch_build()


def _run_initialize(
    root: Path,
    monkeypatch: pytest.MonkeyPatch,
    commit: str,
    *,
    version: str = "0.4.1.dev9+g3e6e47256",
) -> dict[str, list[str]]:
    """在假项目根上跑一次钩子 initialize，返回 build_data。"""
    (root / "wing").mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("WING_COMMIT_HASH", commit)
    metadata = MagicMock()
    metadata.version = version
    hook = hatch_build.BuildInfoHook(str(root), {}, None, metadata, "", "wheel")
    build_data: dict[str, list[str]] = {"artifacts": []}
    hook.initialize("editable", build_data)
    return build_data


class TestResolveCommit:
    """resolve_commit：环境变量优先 → 构建期 git → None。"""

    def test_env_full_sha_truncated_to_short(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        """CI 传入完整 sha（github.sha）——截短到 7 位，与 Rust build.rs 一致。"""
        monkeypatch.setenv(
            "WING_COMMIT_HASH", "3e6e472567729c7a0ae08206ca2aef2766447d83"
        )
        assert hatch_build.resolve_commit(tmp_path) == "3e6e472"

    def test_env_short_value_kept(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        monkeypatch.setenv("WING_COMMIT_HASH", "abc1234")
        assert hatch_build.resolve_commit(tmp_path) == "abc1234"

    def test_blank_env_falls_back_to_git(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """空白环境变量视为未设置 → 走构建期 git rev-parse。"""
        monkeypatch.setenv("WING_COMMIT_HASH", "  ")
        if not (_REPO_ROOT / ".git").exists():
            pytest.skip("not a git checkout")
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=_PACKAGE_ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        assert hatch_build.resolve_commit(_PACKAGE_ROOT) == head[:7]

    def test_no_env_no_git_returns_none(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        """无环境变量且不在 git 仓库（如从 sdist 构建）→ None。"""
        monkeypatch.delenv("WING_COMMIT_HASH", raising=False)
        assert hatch_build.resolve_commit(tmp_path) is None


class TestRenderBuildInfo:
    """render_build_info：生成的模块必须是可导入的合法 Python。"""

    def test_renders_version_and_commit(self) -> None:
        ns: dict[str, object] = {}
        exec(hatch_build.render_build_info("0.4.1.dev9+g3e6e47256", "3e6e472"), ns)
        assert ns["version"] == "0.4.1.dev9+g3e6e47256"
        assert ns["commit"] == "3e6e472"

    def test_renders_none_commit(self) -> None:
        ns: dict[str, object] = {}
        exec(hatch_build.render_build_info("0.4.1", None), ns)
        assert ns["commit"] is None

    def test_render_deterministic(self) -> None:
        """同样输入 → 同样输出（写文件以此判等，内容不变则不重写）。"""
        assert hatch_build.render_build_info(
            "0.4.1", "abc1234"
        ) == hatch_build.render_build_info("0.4.1", "abc1234")


class TestBuildInfoHook:
    """initialize：写生成文件 + 声明为 wheel 产物。"""

    @staticmethod
    def test_writes_generated_file_and_declares_artifact(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        build_data = _run_initialize(
            tmp_path, monkeypatch, "3e6e472567729c7a0ae08206ca2aef2766447d83"
        )
        text = (tmp_path / "wing/_build_info.py").read_text(encoding="utf-8")
        ns: dict[str, object] = {}
        exec(text, ns)
        assert ns["version"] == "0.4.1.dev9+g3e6e47256"
        assert ns["commit"] == "3e6e472"
        assert build_data["artifacts"] == ["/wing/_build_info.py"]

    @staticmethod
    def test_unchanged_content_not_rewritten(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """内容不变则保留原文件——mtime 稳定，避免无谓触发下游重建判定。"""
        _run_initialize(tmp_path, monkeypatch, "abc1234")
        target = tmp_path / "wing/_build_info.py"
        os.utime(target, (1_000_000, 1_000_000))
        _run_initialize(tmp_path, monkeypatch, "abc1234")
        assert target.stat().st_mtime == 1_000_000

    @staticmethod
    def test_changed_commit_rewrites(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """commit 变化（如提交后重装）→ 生成文件刷新。"""
        _run_initialize(tmp_path, monkeypatch, "abc1234")
        _run_initialize(tmp_path, monkeypatch, "def5678")
        assert "def5678" in (tmp_path / "wing/_build_info.py").read_text(
            encoding="utf-8"
        )


class TestBuildInfoReader:
    """wing.build_info：运行期只读生成文件，缺失时返回 None。"""

    def test_values_shape(self) -> None:
        version = build_info.get_version()
        assert version is None or version
        commit = build_info.get_commit()
        assert commit is None or re.fullmatch(r"[0-9a-f]{7}", commit)

    def test_missing_generated_file_returns_none(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """生成文件缺失（未经构建的源码树）→ (None, None)，不抛异常。"""
        monkeypatch.setitem(sys.modules, "wing._build_info", None)
        assert build_info._load_build_info() == (None, None)
