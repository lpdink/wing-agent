"""wing-core 测试共享 fixtures。

所有测试共享：
  - WING_SESSIONS_PATH 设为临时目录，避免测试 session 污染
    用户真实的 ~/.wing/sessions/ 目录。
  - 自动 mock config，避免加载真实 config.yaml（可能使用旧格式）
  - 显式安装内置工具注册与 metrics 订阅（`_install_wing`，见下）——
    顶层 `wing/__init__` 已无 import 副作用（06 步骤）。
"""

import logging
import os
import tempfile
from collections.abc import Callable, Iterator
from unittest.mock import patch

import pytest

from wing.provider.base import ModelProvider


class _CapturingHandler(logging.Handler):
    """捕获 ``wing`` logger 的消息（``propagate=False``，caplog 抓不到）。"""

    def __init__(self) -> None:
        super().__init__(level=logging.WARNING)
        self.messages: list[str] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.messages.append(record.getMessage())


@pytest.fixture
def wing_warnings() -> Iterator[list[str]]:
    """抓 ``wing`` logger 的 WARNING 消息。

    ``wing/common/logger.py`` 在导入时就把 ``propagate`` 关掉（日志只经显式装配
    的文件 handler 出去），因此 pytest 的 ``caplog``（挂在 root 上）看不到任何一条
    后端日志——实测 ``caplog.text`` 为空串。这里直接把捕获 handler 挂到 ``wing``
    logger 上：同一条通道，不依赖 cwd / 配置文件。
    """
    logger = logging.getLogger("wing")
    handler = _CapturingHandler()
    previous_level = logger.level
    logger.addHandler(handler)
    logger.setLevel(logging.WARNING)
    try:
        yield handler.messages
    finally:
        logger.removeHandler(handler)
        logger.setLevel(previous_level)


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
def _isolate_provider_pool(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """每个测试一枚全新 provider 池。

    池是模块级单例（生产语义如此）；实例若是跨测试复用，会携带上一个测试的
    配置快照（base_url / api_key 等）——必须隔离，否则测试之间互相污染。
    """
    import wing.provider.pool as pool_mod

    monkeypatch.setattr(pool_mod, "_pool", pool_mod.ProviderPool())
    yield


@pytest.fixture()
def install_provider() -> Callable[[ModelProvider], ModelProvider]:
    """把测试自建的 provider 实例登记进共享池（按 provider.name 替换懒建条目）。

    agent 只持 provider name、解析一律经池——需要注入定制实例（假 client /
    定制 config）的测试用本 fixture 把实例放回池里，而不是绕过池传递实例。
    （池单例已被 ``_isolate_provider_pool`` 逐测试替换，登记随测试消亡。）
    """
    import wing.provider.pool as pool_mod

    def _install(provider: ModelProvider) -> ModelProvider:
        pool_mod._pool._providers[provider.name] = provider
        pool_mod._pool._configs[provider.name] = provider.config
        return provider

    return _install


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

    # 只替换 loader 单例：get_config()（任何模块持有的引用）都在调用时读它；
    # 额外 patch `wing.config.get_config` 名字反而会让惰性 import 的读取方
    # （如 wing.provider.pool）绕开测试注入的 config。
    with patch("wing.config.loader._config", test_config):
        yield test_config

    reset_config()
