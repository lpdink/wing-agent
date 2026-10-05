"""public API 卫生：顶层 `wing` 无 import 副作用 + 显式安装幂等。

06 步骤把两件安装动作从 import 期副作用改为显式调用：
  - 内置工具注册：`import wing.tools`
  - metrics 订阅：`wing.metrics_registry.install()`（幂等）

「import 有没有副作用」只能在**全新进程**里断言（本测试进程已被 conftest
安装），因此本文件用 `python -c` 子进程做守卫——它是 ``WingRuntime.__init__``
安装契约的可回归版本。
"""

from __future__ import annotations

import subprocess
import sys

# 全新解释器里的安装契约：`import wing` 不安装任何东西；两件安装显式各就各位；
# install() 重复调用不产生第二次订阅（EventBus.subscribe 是 append 语义，
# 重复订阅 = 指标双写）。
_PROBE = """
import wing

from wing.event_bus import event_bus
from wing.tool_registry import tool_registry

assert tool_registry.tools == [], [t.name for t in tool_registry.tools]
assert event_bus.subscriber_count == 0, event_bus.subscriber_count

from wing import metrics_registry  # 导入本身不订阅：安装是显式的

assert event_bus.subscriber_count == 0, event_bus.subscriber_count
assert tool_registry.tools == [], [t.name for t in tool_registry.tools]

import wing.tools  # 显式安装①：内置工具注册（装饰器 + 模块导入）

names = sorted(t.name for t in tool_registry.tools)
assert names == [
    "AskUserQuestion",
    "Bash",
    "Edit",
    "Glob",
    "Grep",
    "Read",
    "ReadImage",
    "TodoWrite",
    "Write",
], names

metrics_registry.install()  # 显式安装②：metrics 订阅
assert event_bus.subscriber_count == 1, event_bus.subscriber_count

metrics_registry.install()  # 幂等：重复调用不重复订阅
assert event_bus.subscriber_count == 1, event_bus.subscriber_count

print("ok")
"""


def test_top_level_import_is_side_effect_free_and_install_is_explicit():
    proc = subprocess.run(
        [sys.executable, "-c", _PROBE],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert proc.returncode == 0, proc.stderr
    assert proc.stdout.strip() == "ok"


def test_top_level_no_longer_reexports_execute_shell():
    """顶层包不再 re-export `execute_shell`（零消费者，安装已显式化）。"""
    import wing

    assert not hasattr(wing, "execute_shell")
