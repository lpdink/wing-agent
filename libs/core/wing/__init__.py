"""wing — agent runtime 核心包。

包入口刻意保持极薄：**不做任何 import 副作用**（不注册内置工具、不订阅
metrics）。两件安装动作都是显式的，归组合根与测试侧：

  - 内置工具注册：``import wing.tools``（模块导入触发装饰器注册）
  - 审计订阅：``wing.audit.install()``

生产落点：``WingRuntime.__init__``（唯一的进程级组装点）；
测试落点：``libs/core/tests/conftest.py``（测试大量直构 SessionManager，
绕过 WingRuntime）。
"""
