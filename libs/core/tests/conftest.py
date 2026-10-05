"""wing-core 测试共享 fixtures。

所有测试共享：
  - WING_SESSIONS_PATH 设为临时目录，避免测试 session 污染
    用户真实的 ~/.wing/sessions/ 目录。
  - 自动 mock config，避免加载真实 config.yaml（可能使用旧格式）
  - 显式安装内置工具注册与 metrics 订阅（`_install_wing`，见下）——
    顶层 `wing/__init__` 已无 import 副作用（06 步骤）。
"""

import os
import tempfile
from unittest.mock import patch

import pytest

# 模块级：整个测试 session 共享一个临时目录
_TEST_SESSIONS_DIR = tempfile.mkdtemp(prefix="wing-test-sessions-")
os.environ["WING_SESSIONS_PATH"] = _TEST_SESSIONS_DIR


@pytest.fixture(scope="session", autouse=True)
def _install_wing() -> None:
    """显式安装内置能力——测试侧的组合根等价物。

    顶层 `wing/__init__` 不做 import 副作用（06 步骤）；测试大量直接构造
    SessionManager / AgentTemplate（不经 WingRuntime），必须在此完成与
    `WingRuntime.__init__` 相同的两件事：
      - `import wing.tools`：装饰器注册内置工具；
      - `wing.audit.install()`：注册 handler 并订阅 EventBus。
    """
    import wing.tools  # noqa: F401 — 导入触发装饰器注册
    from wing.audit import install as install_metrics

    install_metrics()


@pytest.fixture(autouse=True)
def _isolate_sessions(monkeypatch: pytest.MonkeyPatch) -> None:
    """将 session 存储重定向到临时目录。

    autouse fixture，每个测试自动生效。
    """
    monkeypatch.setenv("WING_SESSIONS_PATH", _TEST_SESSIONS_DIR)


@pytest.fixture(autouse=True)
def _mock_config():
    """自动 mock config，避免加载真实 config.yaml。

    提供一个最小化的测试 Config，使用新的 providers 列表格式。
    """
    from wing.config import AgentConfig, Config, ProviderConfig, reset_config

    reset_config()

    test_config = Config(
        providers=[
            ProviderConfig(
                name="default", base_url="https://api.example.com", api_key="test"
            ),
            ProviderConfig(
                name="alt", base_url="https://api.alt.com", api_key="test-alt"
            ),
        ],
        agents=[
            AgentConfig(
                name="default",
                model="gpt-4",
                tools=[
                    "Bash",
                    "Read",
                    "Write",
                    "Glob",
                    "Grep",
                    "AskUserQuestion",
                    "TodoWrite",
                ],
            )
        ],
    )

    with patch("wing.config.get_config", return_value=test_config):
        with patch("wing.config.loader._config", test_config):
            yield test_config

    reset_config()
