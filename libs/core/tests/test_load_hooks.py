"""load_hooks 单元测试。

核心行为：
  1. load_hooks 从 glob 规则列表找到 .py 文件
  2. importlib import 每个 .py 文件
  3. import 失败的文件不 crash，log warning 并跳过
  4. glob 规则支持 ~ 展开
  5. 无 hooks 配置时不做任何事
"""

import tempfile
from pathlib import Path

import pytest

from wing.hooks import load_hooks


@pytest.fixture(autouse=True)
def clean_global_hooks():
    """擦掉进程级 registry：本文件把 hook 文件 import 进全局 ``hooks``，用完必须还原。

    没擦干净的后果不是本文件红，而是**同一 worker 里后续的任意用例**：任何 post 用户
    消息的用例都会被套上 ``[hooked] …`` / ``hook0(…)`` 前缀。串行时靠文件名顺序
    （``test_inbox…`` 在 ``test_load_hooks`` 前面）侥幸躲过，xdist 下文件→worker 的
    分配一变就红（``test_inbox_drain_merge`` 就是这么撞上的）。
    """
    from wing.hooks import hooks as global_hooks

    global_hooks.clear()
    yield
    global_hooks.clear()


class TestLoadHooks:
    def test_no_hooks_config_no_action(self):
        """无 hooks 配置时不做任何事"""
        load_hooks([])

    def test_empty_hooks_list_no_action(self):
        """空 hooks 列表时不做任何事"""
        load_hooks([])

    def test_load_hooks_from_glob_pattern(self):
        """从 glob 规则找到 .py 文件并 import"""
        with tempfile.TemporaryDirectory() as tmpdir:
            hook_file = Path(tmpdir) / "test_hook.py"
            hook_file.write_text(
                "from wing.hooks import hooks\n"
                "def hooked(msg, **ctx):\n"
                "    return '[hooked] ' + msg\n"
                "hooks.on('before_user_message')(hooked)\n"
            )

            load_hooks([str(hook_file)])

            from wing.hooks import hooks as hook_registry

            handlers = hook_registry.handlers("before_user_message")
            assert len(handlers) > 0

    def test_load_hooks_with_wildcard_glob(self):
        """glob 规则支持通配符"""
        with tempfile.TemporaryDirectory() as tmpdir:
            for i in range(3):
                hook_file = Path(tmpdir) / f"hook_{i}.py"
                content = (
                    "from wing.hooks import hooks\n"
                    f"def handler_{i}(msg, **ctx):\n"
                    f"    return 'hook{i}(' + msg + ')'\n"
                    f"hooks.on('before_user_message')(handler_{i})\n"
                )
                hook_file.write_text(content)

            load_hooks([f"{tmpdir}/hook_*.py"])

            from wing.hooks import hooks as hook_registry

            handlers = hook_registry.handlers("before_user_message")
            assert len(handlers) >= 3

    def test_load_hooks_import_failure_does_not_crash(self):
        """import 失败的文件不 crash，跳过"""
        with tempfile.TemporaryDirectory() as tmpdir:
            bad_hook = Path(tmpdir) / "bad_hook.py"
            bad_hook.write_text("import nonexistent_module\n")

            good_hook = Path(tmpdir) / "good_hook.py"
            good_hook.write_text(
                "from wing.hooks import hooks\n"
                "hooks.on('before_user_message')(lambda msg, **ctx: f'[good] {msg}')\n"
            )

            load_hooks([f"{tmpdir}/*.py"])

            from wing.hooks import hooks as hook_registry

            handlers = hook_registry.handlers("before_user_message")
            assert len(handlers) >= 1

    def test_load_hooks_glob_no_matching_files(self):
        """glob 规则没有匹配到任何文件时不 crash"""
        with tempfile.TemporaryDirectory() as tmpdir:
            load_hooks([f"{tmpdir}/nonexistent/*.py"])
