"""public API 卫生：顶层 `wing` 无 import 副作用 + 显式安装幂等 + 组合根真装。

06 步骤把两件安装动作从 import 期副作用改为显式调用：
  - 内置工具注册：`import wing.tools`
  - metrics 订阅：`wing.audit.install()`（幂等）

「import 有没有副作用」「WingRuntime 会不会真安装」都只能在**全新进程**里
断言（本测试进程已被 conftest 的 session fixture 全局安装，同进程无法区分
「runtime 装了」与「conftest 装了」），因此本文件用 `python -c` 子进程做
守卫——它们是 ``WingRuntime.__init__`` 安装契约的可回归版本。
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

from wing import audit  # 导入本身不订阅：安装是显式的

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

audit.install()  # 显式安装②：metrics 订阅
assert event_bus.subscriber_count == 1, event_bus.subscriber_count

audit.install()  # 幂等：重复调用不重复订阅
assert event_bus.subscriber_count == 1, event_bus.subscriber_count

print("ok")
"""

# 组合根安装契约（S1）：全新进程走真实入口链（临时 WING_HOME → 落默认模板 →
# load_config → WingRuntime()），断言两件事在构造后成立：
#   1. 默认模板的工具（9 个）能被解析出来 —— 即工具注册确实在
#      SessionManager/AgentTemplateManager 构造之前完成；
#   2. metrics 订阅恰为 1（仅 metrics；SessionReaper 的订阅在 gateway attach）。
# 只钉「WingRuntime() 会安装」这一侧：这侧在同进程内不可断言（conftest 已装）。
_RUNTIME_PROBE = """
import os
import tempfile
from pathlib import Path

with tempfile.TemporaryDirectory(prefix="wing-runtime-probe-") as tmp:
    os.environ["WING_HOME"] = tmp
    os.environ["WING_SESSIONS_PATH"] = str(Path(tmp) / "sessions")

    from wing.config import load_config
    from wing.config import DEFAULT_CONFIG_YAML

    config_path = Path(tmp) / "core" / "config.yaml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.write_text(DEFAULT_CONFIG_YAML, encoding="utf-8")
    load_config()

    from wing.event_bus import event_bus
    from wing.tool_registry import tool_registry

    # 构造之前：没有安装痕迹（import 链本身不装）
    assert tool_registry.tools == [], [t.name for t in tool_registry.tools]
    assert event_bus.subscriber_count == 0, event_bus.subscriber_count

    from wing.runtime import WingRuntime

    runtime = WingRuntime()

    # 构造之后：工具已注册（模板工具 9/9 解析）且 metrics 订阅恰一次
    tools = runtime.template_manager.default.resolved_tools
    assert len(tools) == 9, sorted(t.name for t in tools)
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


def test_runtime_construction_installs_builtin_tools_and_metrics():
    """组合根 `WingRuntime()` 会真安装（工具注册 + metrics 订阅各就各位）。

    同进程内 conftest 已全局安装，无法区分「runtime 装了」与「conftest
    装了」——因此用子进程走真实入口链（含临时 WING_HOME 与默认模板）。
    """
    proc = subprocess.run(
        [sys.executable, "-c", _RUNTIME_PROBE],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert proc.returncode == 0, proc.stderr
    # load_config() 会打印 "load config from: …"，取最后一个非空行做哨兵。
    assert proc.stdout.strip().splitlines()[-1] == "ok", proc.stdout


def test_top_level_no_longer_reexports_execute_shell():
    """顶层包不再 re-export `execute_shell`（零消费者，安装已显式化）。"""
    import wing

    assert not hasattr(wing, "execute_shell")
